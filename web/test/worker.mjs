// A guest process running in a Worker, off the main thread.
//
//   node web/test/worker.mjs [program ...]
//
// The first step of replacing the process switcher (task #24). What it proves
// is narrow and worth proving: the shim, the runtime translator and a real
// program all work in a Worker with their own `WebAssembly.Memory`, which is
// what gives each process a private address space and so removes the reason
// two processes' libraries collide.
//
// Two processes are started at once and both are checked, because the point
// is that they do not share memory: run separately they would pass whatever
// happens, and the interesting failure is the one where they tread on each
// other.
//
// Node's Worker rather than a browser's, deliberately. The code under test is
// the same file a page loads; what differs is only how a message is posted,
// and Node turns a twenty-minute cycle into a two-minute one.
import { Worker } from 'node:worker_threads';
import { ProcessHost } from '../../shims/browser/js/host.mjs';
import { readFile, access } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..', '..');
const WORKER = join(ROOT, 'shims/browser/js/worker.mjs');
const SHIM = join(ROOT, 'target/wasm32-unknown-unknown/release/gex_browser.wasm');
const SEED = join(ROOT, 'web/public/gex/seed.wasm');

const exists = async (p) => access(p).then(() => true, () => false);
const skip = (why) => {
  console.log(`skip: ${why}`);
  process.exit(0);
};

if (!(await exists(SHIM))) skip('no browser shim (shims/browser/build.sh)');
if (!(await exists(SEED))) skip('no seed module (web/build-assets.sh)');

// `--dir /=<path>` bundles a tree, so a Worker can run a dynamically linked
// program with its libraries -- which is what every real demo is. Without it
// this could only ever prove a static binary works.
let root = null;
const argv = process.argv.slice(2);
const dirAt = argv.indexOf('--dir');
if (dirAt >= 0) {
  root = argv[dirAt + 1].split('=').pop();
  argv.splice(dirAt, 2);
}
// Everything after `--` is the guest's argv, so a shell can be given a
// script rather than an empty stdin.
const dashAt = argv.indexOf('--');
const extra = dashAt >= 0 ? argv.splice(dashAt).slice(1) : [];
const programs = argv;
if (!programs.length) skip('give it one or more aarch64 programs to run');
for (const p of programs) {
  if (!(await exists(p))) skip(`no program at ${p}`);
}

const shim = new Uint8Array(await readFile(SHIM));
const seed = new Uint8Array(await readFile(SEED));

// The global kernel, if it has been built: the process table and the
// filesystem every process shares. Without it a guest that forks gets as far
// as the fork and no further, because the child would have neither a place in
// the process table nor a root to `execve` out of.
const KERNEL = join(ROOT, 'web/public/gex/gex_kernel.plain.wasm');
const kernel = (await exists(KERNEL))
  ? {
      module: await WebAssembly.compile(new Uint8Array(await readFile(KERNEL))),
      memory: new WebAssembly.Memory({ initial: 2048, maximum: 16384, shared: true }),
    }
  : null;

/** Everything under `dir`, as the guest's filesystem. */
async function bundle(dir) {
  const { readdir } = await import('node:fs/promises');
  const files = {};
  const walk = async (at, guestAt) => {
    for (const e of await readdir(at, { withFileTypes: true })) {
      const host = join(at, e.name);
      const guest = `${guestAt}/${e.name}`;
      if (e.isDirectory()) await walk(host, guest);
      else if (e.isFile()) files[guest] = new Uint8Array(await readFile(host));
    }
  };
  await walk(dir, '');
  return files;
}
const files = root ? await bundle(root) : {};

/** Run one program in its own Worker and collect what it said. */
function runInWorker(elfPath, elf, args) {
  let output = '';
  const host = new ProcessHost({
    makeWorker: () => new Worker(WORKER),
    shim,
    guest: seed,
    kernel,
    gexUrl: join(ROOT, 'shims/browser/js/gex.mjs'),
    onOutput: (fd, text) => {
      output += text;
    },
  });
  return host
    .start({ elf, files, args, opts: root ? { cwd: '/', uid: 0 } : {} })
    .then((status) => ({ status, forked: host.forked, output }))
    .catch((e) => {
      host.stop();
      return { threw: e.message, output };
    });
}

const runs = await Promise.all(
  programs.map(async (p) => {
    const elf = new Uint8Array(await readFile(p));
    return [p, await runInWorker(p, elf, [p, ...extra])];
  }),
);

let bad = 0;
for (const [path, r] of runs) {
  const name = path.split('/').pop();
  if (r.threw) {
    console.log(`FAIL worker-${name}: ${r.threw}`);
    bad++;
    continue;
  }
  if (r.status !== 0) {
    console.log(`FAIL worker-${name}: exit ${r.status}`);
    console.log(r.output.split('\n').map((l) => `     ${l}`).join('\n'));
    bad++;
    continue;
  }
  console.log(
    `ok   worker-${name} (own address space${r.forked ? `, ${r.forked} forked` : ''})`,
  );
  for (const line of r.output.trimEnd().split('\n')) console.log(`     ${line}`);
}
process.exit(bad ? 1 : 0);
