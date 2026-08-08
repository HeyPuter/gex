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
  return new Promise((resolve) => {
    const w = new Worker(WORKER);
    let output = '';
    const done = (result) => {
      w.terminate();
      resolve({ ...result, output });
    };
    w.on('message', (m) => {
      switch (m.kind) {
        case 'ready':
          w.postMessage({
            kind: 'run',
            gexUrl: join(ROOT, 'shims/browser/js/gex.mjs'),
            shim,
            guest: seed,
            elf,
            files,
            args,
            opts: root ? { cwd: '/', uid: 0 } : {},
          });
          break;
        case 'output':
          output += m.text;
          break;
        case 'exit':
          done({ status: m.status, blocks: m.blocks, millis: m.millis });
          break;
        case 'threw':
          done({ threw: m.message });
          break;
      }
    });
    w.on('error', (e) => done({ threw: e.message }));
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
    `ok   worker-${name} (${r.blocks} blocks in ${r.millis}ms, own address space)`,
  );
  for (const line of r.output.trimEnd().split('\n')) console.log(`     ${line}`);
}
process.exit(bad ? 1 : 0);
