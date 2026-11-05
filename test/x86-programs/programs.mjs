// x86 guest programs whose behaviour is checked against native execution
// (fixtures/x86-programs): each is built static with zig for x86-64 and
// i386 musl, run natively and under gexc, and its stdout and exit status
// must match. What each program checks is in its source; how it is run is
// here:
//   - mem64-*: a 64-bit guest memory (gexc run --mem64), x86-64 only;
//     sig-regs-threads x86-64 only (its loops are assembly);
//   - *smc*, copy-loop, null-access and sig-regs-threads under the function
//     JIT as well (--fjit, which must leave writable-and-executable code to
//     tier-1's guards); fp-modes under the function tier's C too (--cgen);
//   - the page tier: the x86-64 build under the browser shim in Node
//     (run-node.mjs, the memory64 shared seed) for PAGE's programs;
//     spawn-threads and exec-dethread in the page only (natively gexc's
//     posix_spawn says ENOENT);
//   - mem64-net reaches a server outside the guest (mem64-net-server.mjs,
//     started once per test, its port argv[1]); threads-many runs its first
//     round only under a task cap below 1024, and once more natively under
//     a cap of 160 (the host refusing a thread is EAGAIN to the guest);
//   - smc-race waits between decoding a block and fixing its guard
//     (GEX_TEST_LIFT_DELAY_US), null-access needs the null guard (a debug
//     opt-in, GEX_NULL_GUARD), fp-modes SSE under MXCSR (GEX_MXCSR), mem64-net
//     the page's network of real TCP sockets (GEX_NETWORK);
//   - in the page, page-cross-entry with GEX_TEST_UNPUBLISHED_RETIRE (a
//     retired page numbered with no directory entry), dense-holes with
//     GEX_TEST_DENSE_HOLES (its stderr must say a hole reached the miss
//     stub), served-entries with GEX_TEST_SERVE_UNMERGED (it and its VAL=2
//     build, as ARGV0-b), fork-dirs with no "gex: heal" line.
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readdirSync, readFileSync, renameSync } from 'node:fs';
import { join } from 'node:path';
import { guest, ROOT } from '../lib/guest.mjs';

export const FIX = join(ROOT, 'test/fixtures/x86-programs');
export const PROGRAMS = readdirSync(FIX).filter((f) => f.endsWith('.c') && !f.startsWith('glibc-')).map((f) => f.slice(0, -2)).sort();

export const PAGE = new Set(`fork-dirs dense-lazy dense-holes spawn-threads served-entries page-cross-entry tgkill-futex exec-tail mem64-io mem64-net mem64-console mem64-smc
  mem64-reserve retire-during-lift close-while-reading thread-signals signal-storm exec-signals clock-monotonic main-thread-exit pi-mutex tcp-loopback pwait-sigmask
  sa-restart thread-churn kernel-contend exec-futex sleep-threads sendmsg-rights scm-fork ioctl-threads fdtable-threads mmap-threads codemap-threads proc-threads
  fs-threads sig-state-threads lift-threads fdmisc-threads task-ids sock-threads exec-dethread fork-thread-ctx fork-maps-threads fork-twice-thread quiet-threads
  fdobj-threads cred-threads kill-threads sigwait-threads poll-timeout tid-threads mount-threads exit-threads many-workers threads-many stale-loop-head file-stale-loop
  stale-cross-page xattr mem64-wx mmap-coherent mmap-remap mmap-writeback null-access sig-regs-threads sig-pause fp-modes`.split(/\s+/));
const PAGE_ONLY = new Set(['spawn-threads', 'exec-dethread']);

/** NAME's targets and native tiers (t1, fjit, cgen). */
export function plan(name) {
  const targets = name.startsWith('mem64-') || name === 'sig-regs-threads' ? ['x86_64-linux-musl'] : ['x86_64-linux-musl', 'x86-linux-musl'];
  let tiers = ['t1'];
  if (/smc/.test(name) || ['copy-loop', 'null-access', 'sig-regs-threads'].includes(name)) tiers = ['t1', 'fjit'];
  if (name === 'fp-modes') tiers = ['t1', 'fjit', 'cgen'];
  if (PAGE_ONLY.has(name)) tiers = [];
  return { targets, tiers, mem64: name.startsWith('mem64-') };
}

/** The guest environment NAME runs with (`page`: on the page). */
export function env(name, page) {
  const e = {};
  if (name === 'smc-race') e.GEX_TEST_LIFT_DELAY_US = '300';
  if (name === 'null-access') e.GEX_NULL_GUARD = '1';
  if (name === 'fp-modes') e.GEX_MXCSR = '1';
  if (name === 'mem64-net') e.GEX_NETWORK = '1';
  if (page && name === 'page-cross-entry') e.GEX_TEST_UNPUBLISHED_RETIRE = '1';
  if (page && name === 'dense-holes') e.GEX_TEST_DENSE_HOLES = '1';
  if (page && name === 'served-entries') e.GEX_TEST_SERVE_UNMERGED = '1';
  return e;
}

export function x86Programs(t) {
  const g = guest(t);
  const zig = g.tool('zig');
  const dir = join(t.workDir, 'x86-programs', 'bin');
  mkdirSync(dir, { recursive: true });
  const { NODE_CHANNEL_FD, NODE_CHANNEL_SERIALIZATION_MODE, ...base } = process.env;
  // Output is compared with the native run: not even gex's default lines.
  const hostEnv = { ...base, GEX_LOG: 'off' };
  let server = null;
  const builds = {};
  const x = {
    g,
    dir,
    hostEnv,
    /** NAME built for TARGET (VAL=2 too as -b for served-entries), once per work directory and source. */
    build(name, target) {
      const src = join(FIX, `${name}.c`);
      const key = createHash('sha256').update(readFileSync(src)).digest('hex').slice(0, 12);
      const bin = join(dir, `${name}-${target}-${key}`);
      const one = async (out, more = []) => {
        if (existsSync(out)) return;
        const tmp = `${out}.tmp-${process.pid}`;
        const r = await x.exec([zig, 'cc', '-target', target, '-O1', '-static', ...(target === 'x86-linux-musl' ? ['-Wl,--image-base=0x1000000'] : []), ...more, '-o', tmp, src], { timeout: 300000 });
        if (r.code !== 0) throw new Error(`zig cc ${name} (${target}): ${r.err.trim().split('\n').slice(-3).join(' / ')}`);
        renameSync(tmp, out);
      };
      builds[bin] ??= (async () => {
        await one(bin);
        if (name === 'served-entries') await one(`${bin}-b`, ['-DVAL=2']);
        return bin;
      })();
      return builds[bin];
    },
    /** `argv` as a process: `{ code, out, err }` (stdout, stderr), `code` as a shell gives it. */
    exec(argv, { env: more = {}, timeout = 120000 } = {}) {
      return new Promise((resolve) => {
        const p = spawn(argv[0], argv.slice(1), { cwd: ROOT, env: { ...hostEnv, ...more }, stdio: ['ignore', 'pipe', 'pipe'] });
        const out = [], err = [];
        p.stdout.on('data', (d) => out.push(d));
        p.stderr.on('data', (d) => err.push(d));
        const timer = setTimeout(() => p.kill('SIGKILL'), timeout);
        p.on('close', (code, signal) => {
          clearTimeout(timer);
          const sig = { SIGABRT: 6, SIGKILL: 9, SIGSEGV: 11, SIGBUS: 7, SIGILL: 4, SIGTERM: 15, SIGFPE: 8 }[signal];
          resolve({ code: code ?? 128 + (sig ?? 0), out: Buffer.concat(out).toString().replace(/\n+$/, ''), err: Buffer.concat(err).toString() });
        });
      });
    },
    /** NAME's arguments: mem64-net's server port (the server started once), threads-many's round. */
    async args(name) {
      if (name === 'mem64-net') {
        if (!server) {
          const p = spawn(process.execPath, [join(FIX, 'mem64-net-server.mjs')], { stdio: ['ignore', 'pipe', 'ignore'] });
          server = { p, port: new Promise((resolve) => p.stdout.once('data', (d) => resolve(String(d).split('\n')[0].trim()))) };
        }
        return [await server.port];
      }
      if (name === 'threads-many') {
        try {
          const cg = readFileSync('/proc/self/cgroup', 'utf8').match(/^0::(.*)$/m)?.[1];
          const max = readFileSync(`/sys/fs/cgroup${cg}/pids.max`, 'utf8').trim();
          if (max !== 'max' && Number(max) < 1024) return ['first'];
        } catch {
          // no cgroup v2 limit to read: the full run
        }
      }
      return [];
    },
    /** The server stopped (at the test's end). */
    done() {
      server?.p.kill();
    },
  };
  return x;
}

/** The first lines where `a` and `b` differ, for a FAIL's detail. */
export function differ(a, b) {
  const x = a.split('\n'), y = b.split('\n');
  const i = x.findIndex((l, k) => l !== y[k]);
  return i < 0 && x.length === y.length ? 'same output' : `line ${(i < 0 ? x.length : i) + 1}: native ${JSON.stringify(x[i] ?? '(end)')}, gex ${JSON.stringify(y[i < 0 ? x.length : i] ?? '(end)')}`;
}
