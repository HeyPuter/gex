// Which source a build was made from, and whether that is still the source.
//
// page/dist and page/public/gex are not rebuilt when the source changes, and an
// old build does not fail: it runs the old code, and a test reports what the
// old code does. So each build writes a stamp, `.inputs`, beside what it
// built: one line per artefact, `NAME KEY`, where KEY hashes the content of the
// artefact's inputs (each file's git blob hash as the working tree has it, so
// a commit of what was built keeps the key). Content, not times: a tree
// copied after its build (cp -al, cp --parents) has new mtimes and the same
// key.
//
//   node page/build-inputs.mjs key NAME            the key of NAME's inputs now
//   node page/build-inputs.mjs stamp DIR NAME[=KEY]...
//                                    (re)write NAME's line in DIR/.inputs: KEY,
//                                    taken before the build, or the key now
//
// NAME is an artefact (gex_kernel, gex_browser, page), with the arch after a
// dot for another arch's build (gex_kernel.x86), which has the same inputs.
//
// The harness (gexdev/web/test/harness.mjs) and the test runner (test/run.mjs) call
// refuseStale before they serve anything; GEX_STALE_OK=1 makes a mismatch a
// warning. With no git checkout or no stamp they go on, with a one-line note.
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, readFileSync, realpathSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const ROOT = join(import.meta.dirname, '..');

// What each artefact is built from. The page's JavaScript (`page`) is bundled
// by vite; the kernel and the shim are wasm.
const RUST = ['crates', 'Cargo.toml', 'Cargo.lock'];
const INPUTS = {
  gex_kernel: ['hosts/kernel', ...RUST],
  gex_browser: ['hosts/browser', ':(exclude)hosts/browser/js', ...RUST],
  page: ['page/src', 'page/runtime/src', 'page/index.html', 'page/vite.config.js', 'page/package-lock.json', 'hosts/browser/js'],
};
// Not build inputs anywhere: documentation and tests.
const SKIP = [':(exclude,glob)**/*.md', ':(exclude,glob)**/tests/**', ':(exclude,glob)**/test/**'];

const git = (...args) => execFileSync('git', ['-C', ROOT, ...args], { maxBuffer: 256 << 20, stdio: ['ignore', 'pipe', 'pipe'] });
/** Whether ROOT is a git checkout's top (not a copy without .git, which may
 *  sit inside some other repository). */
let checkout;
function isCheckout() {
  try {
    checkout ??= realpathSync(git('rev-parse', '--show-toplevel').toString().trim()) === realpathSync(ROOT);
  } catch {
    checkout = false;
  }
  return checkout;
}

/** The key of NAME's inputs as this tree has them now; null without git.
 *  Each input file by path and git blob hash of its content in the working
 *  tree: the same files give the same key whether they are committed,
 *  staged, changed or new. */
export function inputsKey(name) {
  const spec = ['--', ...INPUTS[name.split('.')[0]], ...SKIP];
  if (!isCheckout()) return null;
  try {
    const blobs = new Map();
    // Index entries: `MODE BLOB STAGE\tPATH`.
    for (const e of git('ls-files', '-s', '-z', ...spec).toString().split('\0').filter(Boolean)) {
      const tab = e.indexOf('\t');
      blobs.set(e.slice(tab + 1), e.slice(0, tab).split(' ')[1]);
    }
    // Those the working tree changed, and new ones: hashed as they are now
    // (a deleted one is gone from the key).
    const list = (...a) => git(...a, ...spec).toString().split('\0').filter(Boolean);
    const changed = [...new Set([...list('diff', '--name-only', '-z'), ...list('ls-files', '-z', '--others', '--exclude-standard')])];
    const here = changed.filter((f) => existsSync(join(ROOT, f)));
    for (const f of changed) blobs.delete(f);
    if (here.length) {
      const hashes = execFileSync('git', ['-C', ROOT, 'hash-object', '--stdin-paths'], { input: here.join('\n'), maxBuffer: 64 << 20, stdio: ['pipe', 'pipe', 'pipe'] }).toString().trim().split('\n');
      here.forEach((f, i) => blobs.set(f, hashes[i]));
    }
    const h = createHash('sha256');
    for (const f of [...blobs.keys()].sort()) h.update(`${f}\0${blobs.get(f)}\n`);
    return h.digest('hex').slice(0, 32);
  } catch {
    return null;
  }
}

function readStamp(dir) {
  try {
    return new Map(readFileSync(join(dir, '.inputs'), 'utf8').split('\n').filter(Boolean).map((l) => l.split(' ')));
  } catch {
    return null;
  }
}

/** Exit 2 (or, with GEX_STALE_OK=1, warn) when DIR/.inputs says one of
 *  NAMES was built from other source than this tree's. */
export function refuseStale(who, dir, names, rebuild) {
  const stamp = readStamp(dir);
  if (!stamp) return console.error(`${who}: note: ${dir} has no .inputs stamp, so it may be older than its source`);
  const stale = [];
  for (const name of names) {
    if (!stamp.has(name)) { console.error(`${who}: note: ${dir}/.inputs has no ${name} line`); continue; }
    const now = inputsKey(name);
    if (now === null) return console.error(`${who}: note: no git checkout here to check ${dir} against`);
    if (now !== stamp.get(name)) stale.push(name);
  }
  if (!stale.length) return;
  const msg = `${who}: ${dir}: ${stale.join(', ')} built from other source than this tree's (${stale.map((n) => INPUTS[n.split('.')[0]].filter((p) => !p.startsWith(':')).join(' ')).join('; ')}). Rebuild: ${rebuild}`;
  if (process.env.GEX_STALE_OK === '1') return console.error(`${who}: WARNING, STALE BUILD (GEX_STALE_OK=1)\n${msg}`);
  console.error(`${msg}\n${who}: refusing to run old code. GEX_STALE_OK=1 runs it anyway.`);
  process.exit(2);
}

if (process.argv[1] === import.meta.filename) {
  const [cmd, ...args] = process.argv.slice(2);
  const known = (n) => !!INPUTS[n.split('=')[0].split('.')[0]];
  if (cmd === 'key' && args.length === 1 && known(args[0])) {
    console.log(inputsKey(args[0]) ?? '');
  } else if (cmd === 'stamp' && args.length > 1 && args.slice(1).every(known)) {
    const dir = args[0];
    const stamp = readStamp(dir) ?? new Map();
    for (const a of args.slice(1)) {
      const [n, given] = a.split('=');
      const k = given || inputsKey(n);
      if (k) stamp.set(n, k); else stamp.delete(n);
    }
    writeFileSync(join(dir, '.inputs'), [...stamp].map(([n, k]) => `${n} ${k}\n`).join(''));
  } else {
    console.error(`usage: node page/build-inputs.mjs key NAME | stamp DIR NAME[=KEY]... (NAME: ${Object.keys(INPUTS).join(', ')}[.ARCH])`);
    process.exit(2);
  }
}
