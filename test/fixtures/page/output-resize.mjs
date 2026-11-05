// A resize of the page's output reaches Wayland clients that are already
// connected: `mode` then `done` on every wl_output they bound, in the browser
// shim (hosts/browser/src/wl.rs, which asks the page's size every pump
// through `wl_output_size`) and the native one (hosts/native/src/wayland.rs,
// a GEX_WL_INPUT `output W H` line). The viewport goes 1300x713 -> 1000x650
// -> 1300x800, as WD's windemo does. Where the compositor has
// zxdg_output_manager_v1 (crates/gex-wayland), the client also describes its
// output with a zxdg_output_v1 and must see each logical size too (scale 1
// here, so the same sizes; the scaled case is the crate's unit test).
//
//  1. The page's half (hosts/browser/js/wl-bridge.mjs): the size the proxy
//     gives the shim, `inputPending` on a resize (the pump's cue), and the
//     wake of every process with a block when the size changes -- not at the
//     first size, and not for the same size again.
//  2. The browser shim under node (run-node.mjs, the headless compositor with
//     GEX_WL_STUB_OUTPUT's sizes), MP and SP: fixtures/programs/
//     wl-output-resize.c, connected before the first resize, prints each size.
//  3. The native shim (target/x86/release/gexc run --wayland), the same
//     client and sizes.
//
// 2 needs the page's x86 shared build (`GEX_ARCH=x86 GEX_SHARED=1
// hosts/browser/build.sh`) and gcc; 3 needs the x86 gexc. A missing piece is
// a skip, said, not a pass.
//
//   node test/fixtures/page/output-resize.mjs [page|node|native]...
import { pathToFileURL, fileURLToPath } from 'node:url';
import { join, dirname } from 'node:path';
import { existsSync, mkdtempSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { spawnSync } from 'node:child_process';
import assert from 'node:assert/strict';

// gex: the checkout these fixtures are in, or GEX_ROOT.
const GEX_ROOT = process.env.GEX_ROOT || join(import.meta.dirname, '..', '..', '..');
const PROGRAMS = join(import.meta.dirname, '..', 'programs');
const parts = process.argv.slice(2).length ? process.argv.slice(2) : ['page', 'node', 'native'];
const SIZES = [[1300, 713], [1000, 650], [1300, 800]];
const STEP_MS = 2500;
let failed = false;
const ok = (what) => console.log(`ok   ${what}`);
const fail = (what, why) => { console.log(`FAIL ${what}: ${why}`); failed = true; };
const skip = (what, why) => console.log(`skip ${what}: ${why}`);

// -- 1. the page's half --------------------------------------------------------
if (parts.includes('page')) {
  const wl = await import(pathToFileURL(join(GEX_ROOT, 'hosts/browser/js/wl-bridge.mjs')).href);
  const channel = wl.makeWlChannel();
  // The global kernel's memory: one wait queue at 256 with slot 0 registered,
  // the slots' words at 1024 (hosts/kernel/src/wait.rs).
  const kernel = new WebAssembly.Memory({ initial: 1, maximum: 1, shared: true });
  const k = new Int32Array(kernel.buffer);
  wl.setKernelMemory(channel, kernel);
  const proxy = new wl.WlProxy(channel, () => {}, () => 7);
  proxy.wakeAddr = () => ({ addr: 256, slots: 1024 });
  const mem = new WebAssembly.Memory({ initial: 1, maximum: 1, shared: true });
  const pack = (w, h) => ((w << 16) | h) >>> 0;
  try {
    assert.equal(proxy.outputSize(), 0, 'no size before the compositor says one');
    wl.setOutputSize(channel, ...SIZES[0], 1);
    proxy.pollInput(mem, 0, 8); // the block claimed, as a guest's first poll does
    Atomics.or(k, 256 >> 2, 1); // a thread of it waits
    assert.equal(proxy.inputPending(), true, 'a size the pump has not had is work for it');
    assert.equal(proxy.outputSize(), pack(...SIZES[0]));
    assert.equal(proxy.inputPending(), false, 'nothing once it has');
    const woken = () => Atomics.load(k, 1024 >> 2);
    wl.setOutputSize(channel, ...SIZES[0]);
    assert.equal(woken(), 0, 'the same size wakes nobody');
    for (const [i, [w, h]] of SIZES.slice(1).entries()) {
      wl.setOutputSize(channel, w, h);
      assert.equal(woken(), i + 1, `${w}x${h}: the waiting process is woken`);
      assert.equal(proxy.inputPending(), true, `${w}x${h}: pending`);
      assert.equal(proxy.outputSize(), pack(w, h), `${w}x${h}: the size given`);
      assert.equal(proxy.inputPending(), false);
    }
    // A first size (a fresh channel) wakes nobody: no client has had one.
    const fresh = wl.makeWlChannel();
    wl.setKernelMemory(fresh, kernel);
    const p2 = new wl.WlProxy(fresh, () => {}, () => 8);
    p2.wakeAddr = () => ({ addr: 256, slots: 1024 });
    p2.pollInput(mem, 0, 8);
    const before = woken();
    wl.setOutputSize(fresh, 800, 600);
    assert.equal(woken(), before, 'the first size wakes nobody');
    ok('page: the proxy\'s size, its pending flag and the resize\'s wake');
  } catch (e) {
    fail('page', e.message);
  }
}

// -- the client ------------------------------------------------------------------
let work = null;
let client = null;
const want = `${SIZES.at(-1)[0]}x${SIZES.at(-1)[1]}`;
const XDG = readFileSync(join(GEX_ROOT, 'crates/gex-wayland/src/lib.rs'), 'utf8').includes('"zxdg_output_manager_v1"');
// What the client says, binding at size `from`: that size, then each later
// one as a resize. The steps are times after the client's connection, so on
// a loaded machine its bind can come after the first; it must still be
// before the last, and every size after its bind must reach it.
const expect = (from) => SIZES.slice(from).flatMap((s, i) => [`${i ? 'size' : 'bound'} ${s.join('x')}`, ...(XDG ? [`logical ${s.join('x')}`] : [])]);
const expected = expect(0);
function buildClient() {
  if (client) return client;
  if (spawnSync('gcc', ['--version']).status !== 0) return null;
  work ??= mkdtempSync(join(process.env.WORK || tmpdir(), 'output-resize-'));
  const out = join(work, 'wl-output-resize');
  const r = spawnSync('gcc', ['-O1', '-static', '-o', out, join(PROGRAMS, 'wl-output-resize.c')], { encoding: 'utf8' });
  if (r.status !== 0) throw new Error(`gcc: ${r.stderr}`);
  return (client = out);
}
function judge(what, r) {
  const lines = (r.stdout + r.stderr).split('\n');
  const sizes = lines.filter((l) => /^(bound|size|logical) \d+x\d+$/.test(l));
  const from = SIZES.slice(0, -1).findIndex((_, i) => JSON.stringify(sizes) === JSON.stringify(expect(i)));
  if (r.status === 0 && from >= 0) {
    ok(`${what}: ${sizes.join(', ')}${from ? ` (bound after ${from} of the resizes)` : ''}`);
  } else {
    fail(what, `exit ${r.status ?? r.signal}, saw [${sizes.join(', ')}], wanted [${expected.join(', ')}]`);
    console.log(lines.filter((l) => l && !/dispatch table/.test(l)).slice(-10).join('\n'));
  }
}

// -- 2. the browser shim under node ----------------------------------------------
if (parts.includes('node')) {
  const seed = join(GEX_ROOT, 'page/public/gex/seed.x86.shared.wasm');
  const bin = existsSync(seed) ? buildClient() : null;
  if (!bin) {
    skip('node', 'no x86 shared seed or gcc');
  } else {
    const jspi = spawnSync(process.execPath, ['-e', 'process.exit(typeof WebAssembly.Suspending === "function" ? 0 : 1)']).status === 0 ? [] : ['--experimental-wasm-jspi'];
    const plan = SIZES.map(([w, h], i) => `${w}x${h}${i ? `@${i * STEP_MS}` : ''}`).join(',');
    for (const flavour of ['mp', 'sp']) {
      const env = { ...process.env, GEX_WL_STUB: '1', GEX_WL_STUB_OUTPUT: plan, GEX_ARCH: 'x86', GEX_SHARED: '1' };
      if (flavour === 'sp') env.GEX_SINGLE = '1';
      const r = spawnSync('timeout', ['90', process.execPath, ...jspi,
        '--import', 'data:text/javascript,if(process.env.GEX_SINGLE)delete globalThis.SharedArrayBuffer',
        'hosts/browser/js/run-node.mjs', seed, bin, want, '30'],
      { cwd: GEX_ROOT, env, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], maxBuffer: 64 << 20 });
      judge(`node (${flavour}): a connected client sees each size`, r);
    }
  }
}

// -- 3. the native shim ------------------------------------------------------------
if (parts.includes('native')) {
  const gexc = process.env.GEXC || join(GEX_ROOT, 'target/x86/release/gexc');
  const bin = existsSync(gexc) ? buildClient() : null;
  if (!bin) {
    skip('native', `no ${gexc} or gcc`);
  } else {
    // The clock starts when the client connects, where the shim can say so
    // (`clients 1`): from the compositor's start (`frames 0`) it also counts
    // the client's load, which on a loaded machine outlasts the first resize.
    const clients = readFileSync(join(GEX_ROOT, 'hosts/native/src/wayland.rs'), 'utf8').includes('w[0] == "clients"');
    const script = join(work, 'input');
    writeFileSync(script, [clients ? 'clients 1' : 'frames 0', ...SIZES.slice(1).map(([w, h], i) => `${((i + 1) * STEP_MS) / 1000} output ${w} ${h}`)].join('\n') + '\n');
    const r = spawnSync('timeout', ['90', gexc, 'run', '--wayland', '/tmp/wayland-0', '--wayland-size', SIZES[0].join('x'), bin, want, '30'],
      { env: { ...process.env, GEX_WL_INPUT: script }, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], maxBuffer: 64 << 20 });
    judge('native: a connected client sees each size', r);
  }
}

if (work) rmSync(work, { recursive: true, force: true });
process.exit(failed ? 1 : 0);
