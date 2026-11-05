// The page's drop of host files on a guest window (page/src/drag.js over
// hosts/browser/js/wl-bridge.mjs): the events the page sends the window's
// process, what the guest says back through its block, and the drag's
// text/uri-list. A stand-in guest answers as the shim's compositor does
// (hosts/browser/src/wl.rs `answer_drag`) for a program like Wine's: at the
// enter it reads the list and accepts, or never does; at the drop it says
// it took it.
//
//   node test/fixtures/page/drag.mjs
import { pathToFileURL } from 'node:url';
import { join } from 'node:path';
import assert from 'node:assert/strict';
// gex: the checkout these fixtures are in, or GEX_ROOT.
const GEX_ROOT = process.env.GEX_ROOT || join(import.meta.dirname, '..', '..', '..');
const wl = await import(pathToFileURL(join(GEX_ROOT, 'hosts/browser/js/wl-bridge.mjs')).href);
const { PageDrag, uriList } = await import(pathToFileURL(join(GEX_ROOT, 'page/src/drag.js')).href);

const channel = wl.makeWlChannel();
const proxy = new wl.WlProxy(channel, () => {}, () => 7);
const mem = new WebAssembly.Memory({ initial: 4, maximum: 4, shared: true });
const seen = [];
let accept = true;
let data = null;
// The guest's pump: its ring drained, each drag event answered.
const pump = () => {
  const n = proxy.pollInput(mem, 0, 64);
  const ev = new Int32Array(mem.buffer, 0, n * wl.EV);
  for (let i = 0; i < n; i++) {
    const [kind, a, b, c, , client, surface] = ev.subarray(i * wl.EV, i * wl.EV + wl.EV);
    seen.push({ kind, a, b, c, client, surface });
    if (kind === wl.EV_DRAG_ENTER) {
      const len = proxy.dragData(mem, 4096, 60000);
      data = new TextDecoder().decode(new Uint8Array(mem.buffer, 4096, len).slice());
      if (accept) proxy.dragStatus(c, wl.DRAG_ACCEPTS);
    }
    if (kind === wl.EV_DROP) proxy.dragStatus(a, accept ? wl.DRAG_ACCEPTS | wl.DRAG_ANSWERED | wl.DRAG_TAKEN : wl.DRAG_ANSWERED);
  }
};
pump(); // the block claimed, as a guest's first poll does
const timer = setInterval(pump, 5);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
/** Until `ok()`, or 10 s: the pump's turns are late on a loaded machine. */
const until = async (ok) => { for (const t1 = performance.now() + 10000; !ok() && performance.now() < t1;) await sleep(5); };

const drag = new PageDrag(channel, wl);
const win = { block: 0, client: 2, surface: 9, x: 10, y: 20 };
const paths = ['/tmp/drops/1/a b.txt', '/tmp/drops/1/café', '/tmp/drops/1/x%y'];

// Taken: an enter (with the list there already), a motion, the drop.
assert.equal(await drag.drop(win, paths), true, 'taken');
assert.deepEqual(seen.map((e) => e.kind), [wl.EV_DRAG_ENTER, wl.EV_DRAG_MOTION, wl.EV_DROP]);
assert.deepEqual([seen[0].a, seen[0].b, seen[0].client, seen[0].surface], [10, 20, 2, 9], 'enter: point, client, surface');
assert.equal(seen[2].a, seen[0].c, 'the drop names the enter\'s serial');
assert.equal(data, 'file:///tmp/drops/1/a%20b.txt\r\nfile:///tmp/drops/1/caf%C3%A9\r\nfile:///tmp/drops/1/x%25y\r\n', 'the list, read at the enter');
assert.equal(data, uriList(paths));

// A program that never accepts: a leave, no drop, not taken.
accept = false;
seen.length = 0;
assert.equal(await drag.drop(win, paths), false, 'refused');
await until(() => seen.length >= 3);
assert.deepEqual(seen.map((e) => e.kind), [wl.EV_DRAG_ENTER, wl.EV_DRAG_MOTION, wl.EV_DRAG_LEAVE], 'no drop sent');
accept = true;

// A guest that refuses at once (no data device there): answered, no wait.
seen.length = 0;
const t0 = performance.now();
const refuse = setInterval(() => {
  const s = seen.find((e) => e.kind === wl.EV_DRAG_ENTER);
  if (s) { clearInterval(refuse); proxy.dragStatus(s.c, wl.DRAG_ANSWERED); }
}, 1);
accept = false;
assert.equal(await drag.drop(win, paths), false);
assert.ok(performance.now() - t0 < 1000, 'a refusal at the enter is not waited out');
clearInterval(refuse);
accept = true;

// No window, or nothing to drop: nothing sent.
await sleep(20);
seen.length = 0;
assert.equal(await drag.drop(null, paths), false);
assert.equal(await drag.drop(win, []), false);
await sleep(20);
assert.deepEqual(seen, []);

// Data past the arena's room is refused at the page, before an enter is sent.
assert.equal(wl.setDragData(channel, 'x'.repeat(70000)), false);
assert.equal(await drag.drop(win, ['/' + 'x'.repeat(70000)]), false);
await sleep(20);
assert.deepEqual(seen, []);

clearInterval(timer);
console.log('ok   drag: a drop on a guest window, its enter/motion/drop/leave, the answer, the uri-list (5 cases)');
