#!/usr/bin/env node
// Mesa's files are fetched once each when the GL starts (mesa-source.js): the
// packed module and cold part from the pack, never as plain files, and the
// glue, which is not packed, as its plain file once, however many ask. The
// device is handed them all, and still is when the build has no cold part.
// It was handed none when the glue was not packed, and fetched the plain
// module and cold part (20 MB, not 3.5) and the glue once per thread.
//
//   node test/fixtures/page/mesa-source.mjs     prints "ok ..." lines or FAIL
import { pathToFileURL } from 'node:url';
import { join } from 'node:path';
const GEX_ROOT = process.env.GEX_ROOT || join(import.meta.dirname, '..', '..', '..');
const { mesaSource, mesaSources, MESA_FILES } = await import(pathToFileURL(join(GEX_ROOT, 'page/src/mesa-source.js')).href);

let bad = 0;
const check = (name, ok, why = '') => { console.log(`${ok ? 'ok  ' : 'FAIL'} mesa source: ${name}${ok ? '' : ` (${why})`}`); if (!ok) bad++; };

function site({ cold = true } = {}) {
  const plain = [], packed = [];
  const pack = { '/gex/gex-wgpu-replay.wasm': { h: 'a' }, ...(cold ? { '/gex/gex-wgpu-replay.deferred.wasm': { h: 'b' } } : {}) };
  const at = (rel) => `https://site.test/${rel}`;
  const entry = async (href) => pack[new URL(href).pathname] ?? null;
  const fromPack = async (href) => { packed.push(new URL(href).pathname); return new Uint8Array([1]); };
  const fetch = async (href) => {
    const p = new URL(href).pathname;
    plain.push(p);
    const there = p === '/gex/gex-wgpu-replay.js' || pack[p];
    return { ok: !!there, status: there ? 200 : 404, arrayBuffer: async () => new ArrayBuffer(4) };
  };
  return { plain, packed, source: mesaSource(at, entry, fromPack, { fetch }) };
}

{
  const s = site();
  // The device asks, the page asks, and each of Mesa's threads asks for the glue.
  const all = await Promise.all([mesaSources(s.source), mesaSources(s.source), ...Array.from({ length: 5 }, () => s.source('gex-wgpu-replay.js'))]);
  check('the device is handed every file', all[0] && MESA_FILES.every((n) => all[0][n]), JSON.stringify(all[0] && Object.keys(all[0])));
  check('the glue is fetched as its file once', s.plain.join() === '/gex/gex-wgpu-replay.js', s.plain.join());
  check('the module and cold part come from the pack, once each', s.packed.sort().join() === '/gex/gex-wgpu-replay.deferred.wasm,/gex/gex-wgpu-replay.wasm', s.packed.join());
}
{
  const s = site({ cold: false });
  const got = await mesaSources(s.source);
  check('with no cold part the device is still handed the glue and module', !!got && !!got['gex-wgpu-replay.js'] && !!got['gex-wgpu-replay.wasm'] && got['gex-wgpu-replay.deferred.wasm'] === null, JSON.stringify(got && Object.keys(got)));
  check('no plain module is fetched', !s.plain.includes('/gex/gex-wgpu-replay.wasm'), s.plain.join());
}
process.exit(bad ? 1 : 0);
