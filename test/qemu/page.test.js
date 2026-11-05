// AArch64 programs (fixtures/integration) on the page's runtime under Node
// (hosts/browser/js/run-node.mjs: the kernel in wasm, one memory with the
// guest's module), checked against QEMU (lib/qemu.mjs). Each both ways a page
// gets code: a module with every block translated ahead of time (gexc build
// --exhaustive) and a small one that pulls the rest in through the dispatch
// miss. stdout is the compare (the runtime narrates on stderr), stderr too
// with the narration out for the suite's sys area. The conformance suite, the
// VFS, signals, SIMD, flags and loops, C++ exceptions; threads (each a Worker
// on one shared memory); dynamically linked programs (musl's loader
// translated, every library through the miss, bundled with --file: a page has
// no filesystem to reach through). Then on the seed: a syscall parked while
// another process runs and woken by a host timeout, a socket between a parent
// and a child in two Workers, and a parent polling with WNOHANG being told
// "not yet", not ECHILD (how `timeout` waits).
import { readdirSync } from 'node:fs';
import { join } from 'node:path';
import { qemu, unnarrated } from '../lib/qemu.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 1500 };

export default async function (t) {
  const q = qemu(t);
  const { n } = q;
  const g = n.g;
  const np = ['-static', '-no-pie', '-O2'];
  const build = async (wasm, prog, flags) => {
    const r = await n.gexc(['build', ...flags, prog, '-o', wasm]);
    if (r.code !== 0) throw new Error(`gexc build ${prog}: ${(r.stdout + r.stderr).trim().split('\n').slice(-3).join(' / ')}`);
  };
  const differs = (a, b) => {
    const x = a.split('\n'), y = b.split('\n');
    const i = x.findIndex((l, k) => l !== y[k]);
    return `line ${(i < 0 ? x.length : i) + 1}: qemu ${JSON.stringify(x[i < 0 ? x.length : i] ?? null)}, page ${JSON.stringify(y[i < 0 ? x.length : i] ?? null)}`;
  };

  /** `label` ahead of time and through the miss, against QEMU. */
  const page = async (label, prog, args = [], { stderr = false, shared = false } = {}) => {
    const want = await q.qemu(prog, args);
    const mem = shared ? [] : ['--no-shared-memory'];
    for (const [mode, flags] of [['aot', ['--exhaustive', ...mem]], ['jit', mem]]) {
      const name = mode === 'aot' ? `page-${label}` : `page-jit-${label}`;
      const wasm = join(q.dir, `${label}.${mode}.wasm`);
      await build(wasm, prog, flags);
      const r = await g.run(wasm, prog, args, { env: shared ? { GEX_SHARED: '1' } : {}, cwd: q.dir });
      if (r.stdout !== want.stdout) { t.check(name, false, `${differs(want.stdout, r.stdout)}; stderr ${r.stderr.trim().split('\n').slice(-3).join(' / ')}`); continue; }
      t.check(name, !stderr || unnarrated(want.stderr) === unnarrated(r.stderr), stderr ? differs(unnarrated(want.stderr), unnarrated(r.stderr)) : '');
    }
  };

  const p = {
    numeric: await q.build('numeric', 'numeric.c', ['-static', '-no-pie', '-O3', '-ffast-math', '-lm']),
    fixedpoint: await q.build('fixedpoint', 'fixedpoint.c', np),
    vector: await q.build('vector', 'vector.c', ['-static', '-no-pie', '-O3']),
    'neon-structure': await q.build('neon2', 'neon2.c', np),
    'neon-glibc': await q.build('neon3', 'neon3.c', np),
    'neon-varshift': await q.build('neon-varshift', 'neon-varshift.c', np),
    'neon-move': await q.build('neon-move', 'neon-move.c', np),
    'cross-block-flags': await q.build('flags', 'flags.S', ['-nostdlib', '-static', '-no-pie']),
    loops: await q.build('loops', 'loops.c', ['-O2', '-static']),
  };
  for (const [label, prog] of Object.entries(p)) await page(label, prog);
  const suite = await q.build('suite', 'suite.c', np);
  for (const a of ['arith', 'float', 'mem', 'flow']) await page(`suite-${a}`, suite, [a]);
  await page('suite-sys', suite, ['sys'], { stderr: true });
  const signals = await q.build('signals', 'signals.c', np);
  await page('vfs', await q.build('vfs', 'vfs.c', np));
  await page('signal-all', signals, ['all']);
  await page('threads', await q.build('threads', 'threads.c', [...np, '-lpthread']), [], { shared: true });
  await page('thread-exit-race', await q.build('exitrace', 'exitrace.c', [...np, '-lpthread']), [], { shared: true });
  await page('sig-pause', await q.build('sig-pause', 'sig-pause.c', [...np, '-lpthread']), [], { shared: true });
  await page('cpp', await q.buildCxx('cpp', 'cpp.cc', np));

  // Dynamic: through the miss only (the loader's and the libraries' code
  // exists only once translation reaches it).
  const root = q.sysroot();
  const bundle = Object.fromEntries(readdirSync(join(root, 'lib')).map((f) => [`/lib/${f}`, join(root, 'lib', f)]));
  const dynamic = async (label, prog, args = []) => {
    const want = await q.qemu(prog, args, { flags: ['-L', root] });
    const wasm = join(q.dir, `${label}.dyn.wasm`);
    await build(wasm, prog, ['--no-shared-memory']);
    const r = await g.run(wasm, prog, args, { files: bundle, cwd: q.dir });
    t.check(`page-dynamic-${label}`, r.stdout === want.stdout, `${differs(want.stdout, r.stdout)}; stderr ${r.stderr.trim().split('\n').slice(-3).join(' / ')}`);
  };
  const dsuite = await q.build('dyn-suite', 'suite.c', ['-O2']);
  for (const a of ['arith', 'float', 'mem', 'flow']) await dynamic(`suite-${a}`, dsuite, [a]);
  await dynamic('vector', await q.build('dyn-vector', 'vector.c', ['-O2']));
  await dynamic('cpp', await q.buildCxx('dyn-cpp', 'cpp.cc', ['-O2']));

  // On the seed.
  const seed = g.seed({ shared: false });
  const onSeed = async (label, src, want, timeout) => {
    const prog = await q.build(label, src, np);
    const r = await g.run(seed, prog, [], { timeout, cwd: q.dir });
    const all = r.stdout + r.stderr;
    const missing = want.filter((w) => !all.includes(w));
    t.check(label, missing.length === 0, `missing ${JSON.stringify(missing)}: ${all.trim().split('\n').slice(-5).join(' / ')}`);
  };
  await onSeed('park-across-switch', 'park-across-switch.c', ['child: epoll_wait returned 0', 'child: first event 0'], 120000);
  await onSeed('socket-fork', 'socket-fork.c', ['server: got ping', 'child: got pong', 'child exited 0'], 300000);
  await onSeed('wait-nohang', 'wait-after-group.c', ['parent: collected 1 after polling 1, exited 7'], 120000);
}
