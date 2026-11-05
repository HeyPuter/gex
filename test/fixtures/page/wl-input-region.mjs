/**
 * Does a surface with an empty input region let the pointer through?
 *
 * A client publishes three surfaces through `WlProxy`, as the shim does: one
 * with no input region, one whose region is empty (`wl_surface.set_input_region`
 * of an empty `wl_region`, which is how a client lays a subsurface for its GL
 * picture over its parent and keeps the pointer on the parent), and one whose
 * region is a rectangle. The compositor's view (`readScene`) must carry each
 * region, and `takesPointer` must refuse only the empty one: the page's
 * DOM desktop makes that surface transparent to the pointer by it.
 *
 * Usage: node test/fixtures/page/wl-input-region.mjs
 */
import { pathToFileURL as gexPathToFileURL } from 'node:url';
import { join as gexJoin } from 'node:path';
// gex: the checkout these fixtures are in, or GEX_ROOT.
const GEX_ROOT = process.env.GEX_ROOT || gexJoin(import.meta.dirname, '..', '..', '..');
const gexUrl = (p) => gexPathToFileURL(gexJoin(GEX_ROOT, p)).href;
const {
  makeWlChannel, WlProxy, readScene, takesPointer,
  REC, R_CLIENT, R_SURFACE, R_ROLE, R_WIDTH, R_HEIGHT, R_FLAGS,
  R_INPUT_X, R_INPUT_Y, R_INPUT_W, R_INPUT_H, F_INPUT_REGION,
  ROLE_TOPLEVEL, ROLE_SUBSURFACE,
} = await import(gexUrl('hosts/browser/js/wl-bridge.mjs'));

const channel = makeWlChannel();
const proxy = new WlProxy(channel, () => {}, () => 7);
const mem = new WebAssembly.Memory({ initial: 1, maximum: 1, shared: true });
const recs = new Int32Array(mem.buffer, 0, 3 * REC);
const put = (i, surface, role, region) => {
  const o = i * REC;
  recs[o + R_CLIENT] = 1;
  recs[o + R_SURFACE] = surface;
  recs[o + R_ROLE] = role;
  recs[o + R_WIDTH] = 800;
  recs[o + R_HEIGHT] = 500;
  if (region) {
    recs[o + R_FLAGS] |= F_INPUT_REGION;
    [recs[o + R_INPUT_X], recs[o + R_INPUT_Y], recs[o + R_INPUT_W], recs[o + R_INPUT_H]] = region;
  }
};
put(0, 10, ROLE_TOPLEVEL, null);
put(1, 11, ROLE_SUBSURFACE, [0, 0, 0, 0]);
put(2, 12, ROLE_SUBSURFACE, [10, 20, 30, 40]);
proxy.publish(mem, 0, 3);

const { surfaces } = readScene(channel);
const by = new Map(surfaces.map((s) => [s.surface, s]));
const fail = [];
if (surfaces.length !== 3) fail.push(`${surfaces.length} surfaces read, want 3`);
if (by.get(10)?.input !== null) fail.push(`no region read as ${JSON.stringify(by.get(10)?.input)}`);
if (JSON.stringify(by.get(12)?.input) !== JSON.stringify({ x: 10, y: 20, w: 30, h: 40 }))
  fail.push(`a rectangle read as ${JSON.stringify(by.get(12)?.input)}`);
if (!takesPointer(by.get(10))) fail.push('a surface with no input region refuses the pointer');
if (takesPointer(by.get(11))) fail.push('a surface with an empty input region takes the pointer');
if (!takesPointer(by.get(12))) fail.push('a surface with a rectangle input region refuses the pointer');
if (fail.length) {
  for (const f of fail) console.error(`  ${f}`);
  console.error('wl input region check FAILED');
  process.exit(1);
}
console.log('  an empty input region lets the pointer through; none and a rectangle take it');
