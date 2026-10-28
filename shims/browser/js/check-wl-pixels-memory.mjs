/**
 * Are a surface's pixels copied out of the memory its address is in?
 *
 * The host X server publishes a scene its Wayland server module wrote into
 * that module's own memory, while the windows' pixels are in the X server's
 * memory. `WlProxy.publish` takes the two memories apart for that. A client
 * publishes one 4x2 surface whose record is in one memory and whose pixels
 * are at the same address in another, which holds different bytes: the
 * block's pixel buffer must hold the second memory's bytes at the address
 * the compositor's view (`readScene`) gives.
 *
 * Usage: node check-wl-pixels-memory.mjs
 */
import {
  makeWlChannel, WlProxy, readScene,
  REC, R_CLIENT, R_SURFACE, R_ROLE, R_WIDTH, R_HEIGHT, R_ADDR, R_STRIDE,
  R_SERIAL, R_DAMAGE_W, R_DAMAGE_H, ROLE_TOPLEVEL,
} from './wl-bridge.mjs';

const W = 4, H = 2, AT = 4096;
const channel = makeWlChannel();
const proxy = new WlProxy(channel, () => {}, () => 7);
let buffer = null;
proxy.onPixels = (p) => { buffer = p.buffer; };
const scene = new WebAssembly.Memory({ initial: 1, maximum: 1, shared: true });
const pixels = new WebAssembly.Memory({ initial: 1, maximum: 1, shared: true });
const rec = new Int32Array(scene.buffer, 0, REC);
rec[R_CLIENT] = 1;
rec[R_SURFACE] = 10;
rec[R_ROLE] = ROLE_TOPLEVEL;
rec[R_WIDTH] = W;
rec[R_HEIGHT] = H;
rec[R_ADDR] = AT;
rec[R_STRIDE] = W * 4;
rec[R_SERIAL] = 1;
rec[R_DAMAGE_W] = W;
rec[R_DAMAGE_H] = H;
// The scene's memory has nothing at the address; the pixel memory a pattern.
const want = new Uint8Array(pixels.buffer, AT, W * H * 4);
for (let i = 0; i < want.length; i++) want[i] = (i * 7 + 1) & 255;
proxy.publish(scene, 0, 1, pixels);

const fail = [];
const s = readScene(channel).surfaces[0];
if (!s) fail.push('no surface read');
else if (!buffer) fail.push('no pixel buffer made');
else {
  const got = new Uint8Array(buffer, s.addr, W * H * 4);
  const bad = want.findIndex((b, i) => got[i] !== b);
  if (bad >= 0) fail.push(`pixel byte ${bad} is ${got[bad]}, want ${want[bad]} (copied from the wrong memory?)`);
}
if (fail.length) {
  for (const f of fail) console.error(`  ${f}`);
  console.error('wl pixels memory check FAILED');
  process.exit(1);
}
console.log('  pixels copied from the memory their address is in, not the records\'');
