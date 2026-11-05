/**
 * Does a publish merge damage the compositor has not drawn yet, and copy
 * exactly the damaged pixels?
 *
 * `WlProxy.publish` keeps, per surface, the damage of every publish since
 * the compositor last drew (`ackBlock`), and copies a surface's pixels into
 * its block's pixel buffer: all of them the first time, then the damaged
 * rows. One process publishes two surfaces, one packed and one with padded
 * rows:
 *
 *   - two publishes before a draw: the record holds the rectangle covering
 *     both damages;
 *   - after a draw: only the new damage;
 *   - damage of "everything" (w = 0) stays everything until a draw;
 *   - the pixel buffer holds the surface's pixels after changes inside the
 *     damage, and not the changes outside it;
 *   - input events come back in order.
 *
 * Usage: node test/fixtures/page/wl-publish.mjs
 */
import { pathToFileURL as gexPathToFileURL } from 'node:url';
import { join as gexJoin } from 'node:path';
// gex: the checkout these fixtures are in, or GEX_ROOT.
const GEX_ROOT = process.env.GEX_ROOT || gexJoin(import.meta.dirname, '..', '..', '..');
const gexUrl = (p) => gexPathToFileURL(gexJoin(GEX_ROOT, p)).href;
const {
  makeWlChannel, WlProxy, readScene, ackBlock, sendEvent,
  REC, EV, R_CLIENT, R_SURFACE, R_ROLE, R_WIDTH, R_HEIGHT, R_ADDR, R_STRIDE,
  R_SERIAL, R_DAMAGE_X, R_DAMAGE_Y, R_DAMAGE_W, R_DAMAGE_H, ROLE_TOPLEVEL,
} = await import(gexUrl('hosts/browser/js/wl-bridge.mjs'));

const channel = makeWlChannel();
const proxy = new WlProxy(channel, () => {}, () => 7);
let buffer = null;
proxy.onPixels = (p) => { buffer = p.buffer; };
const mem = new WebAssembly.Memory({ initial: 16, maximum: 16, shared: true });
const m8 = new Uint8Array(mem.buffer), m32 = new Int32Array(mem.buffer);
const surfaces = [
  { s: 10, w: 40, h: 30, stride: 160, addr: 65536 },
  { s: 11, w: 100, h: 20, stride: 416, addr: 131072 },
];
for (let i = 0; i < m8.length; i++) m8[i] = (i * 13 + 5) & 255;
let serial = 1;
const publish = (damage) => {
  serial++;
  surfaces.forEach((f, i) => {
    const o = i * REC;
    m32.fill(0, o, o + REC);
    m32[o + R_CLIENT] = 1; m32[o + R_SURFACE] = f.s; m32[o + R_ROLE] = ROLE_TOPLEVEL;
    m32[o + R_WIDTH] = f.w; m32[o + R_HEIGHT] = f.h; m32[o + R_ADDR] = f.addr; m32[o + R_STRIDE] = f.stride;
    m32[o + R_SERIAL] = serial;
    [m32[o + R_DAMAGE_X], m32[o + R_DAMAGE_Y], m32[o + R_DAMAGE_W], m32[o + R_DAMAGE_H]] = damage[i];
  });
  proxy.publish(mem, 0, surfaces.length);
};
const damageOf = () => readScene(channel).surfaces.map((s) => [s.damage?.x ?? s.damageX, s.damage?.y ?? s.damageY, s.damage?.w ?? s.damageW, s.damage?.h ?? s.damageH].join());
const draw = () => ackBlock(channel, 0, proxy.generation);
let bad = 0;
const check = (what, ok, more = '') => {
  if (!ok) bad++;
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}${ok ? '' : ` ${more}`}`);
};
/** Whether the buffer holds surface `i`'s pixels, packed. */
const same = (i) => {
  const f = surfaces[i], s = readScene(channel).surfaces[i];
  const got = new Uint8Array(buffer, s.addr, f.w * f.h * 4);
  for (let y = 0; y < f.h; y++) for (let x = 0; x < f.w * 4; x++) if (got[y * f.w * 4 + x] !== m8[f.addr + y * f.stride + x]) return false;
  return true;
};

publish([[0, 0, 0, 0], [0, 0, 0, 0]]);
check('first publish: every pixel copied', same(0) && same(1));
draw();
publish([[2, 3, 4, 5], [10, 1, 5, 2]]);
publish([[20, 10, 3, 3], [0, 0, 0, 0]]);
const d1 = damageOf();
check('two publishes before a draw: damage merged', d1[0] === '2,3,21,10', `(${d1[0]})`);
check('everything stays everything', d1[1] === '0,0,0,0', `(${d1[1]})`);
draw();
// Changes inside the damage arrive; one outside it does not.
const f = surfaces[0];
m8[f.addr + 6 * f.stride + 8] ^= 0xff;
const outside = f.addr + 25 * f.stride + 30 * 4;
const before = m8[outside];
m8[outside] ^= 0xff;
publish([[1, 5, 5, 2], [3, 4, 2, 2]]);
check('after a draw: only the new damage', damageOf()[0] === '1,5,5,2', `(${damageOf()[0]})`);
m8[outside] = before;
check('damaged pixels copied, others left', same(0));
for (let k = 0; k < 5; k++) sendEvent(channel, 0, 3, k, 2 * k, 0, 0);
const n = proxy.pollInput(mem, 1 << 19, 8);
const evs = [...m32.subarray(1 << 17, (1 << 17) + n * EV)].filter((_, i) => i % EV === 1).join();
check('input events in order', n === 5 && evs === '0,1,2,3,4', `(${n}: ${evs})`);
console.log(bad ? `${bad} failed` : 'publishes merge undrawn damage and copy the damaged pixels');
process.exit(bad ? 1 : 0);
