// Which pages' wineservers keep their periodic registry saves
// (page/src/wine-env.js): only a page whose root is kept (a filesystem other
// than MemoryFilesystem, which keeps nothing).
//
//   node test/fixtures/page/regsave-env.mjs
import { pathToFileURL } from 'node:url';
import { join } from 'node:path';
import assert from 'node:assert/strict';
// gex: the checkout these fixtures are in, or GEX_ROOT.
const GEX_ROOT = process.env.GEX_ROOT || join(import.meta.dirname, '..', '..', '..');
const { noRegSave } = await import(pathToFileURL(join(GEX_ROOT, 'page/src/wine-env.js')).href);
const off = (env, kept) => noRegSave([...env], kept).includes('WINEGEX_NO_REGSAVE=1');
const pfx = ['WINEPREFIX=/pfx'];
assert.equal(off(pfx, false), true, 'a root kept nowhere: no periodic saves');
assert.equal(off(pfx, null), true, 'no filesystem said: no periodic saves');
assert.equal(off(pfx, true), false, 'a kept root: saves');
assert.equal(off([], false), false, 'no WINEPREFIX: nothing set');
assert.deepEqual(noRegSave(['WINEPREFIX=/pfx', 'WINEGEX_NO_REGSAVE=0'], false), ['WINEPREFIX=/pfx', 'WINEGEX_NO_REGSAVE=0'], "the caller's own value stays");
console.log('ok   regsave env: periodic registry saves only for a kept root (5 cases)');
