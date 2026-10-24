// The X server's modules over HTTP (web/src/hostx.js moduleBytes): a `.wasm`
// with a `.wasm.zst` beside it is fetched as the .zst and decoded to the same
// bytes; one without (a 404, or a static server's index page in its place) is
// fetched plain. `node web/test/hostwl-zst.mjs`; prints "hostwl-zst ok".
import { createServer } from 'node:http';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { execFileSync } from 'node:child_process';
import { moduleBytes } from '../src/hostx.js';

const dir = mkdtempSync(join(tmpdir(), 'hostwl-zst-'));
const body = new Uint8Array(3 << 20);
for (let i = 0; i < body.length; i++) body[i] = (i * 2654435761) >>> 24 & (i & 0x100 ? 0xff : 0x0f);
body.set([0x00, 0x61, 0x73, 0x6d, 1, 0, 0, 0]);
for (const n of ['z', 'plain', 'fallback']) writeFileSync(join(dir, `${n}.wasm`), body);
execFileSync('zstd', ['-q', '-19', join(dir, 'z.wasm'), '-o', join(dir, 'z.wasm.zst')]);
const asked = [];
const server = createServer((req, res) => {
  asked.push(req.url);
  const name = req.url.slice(1);
  if (name === 'fallback.wasm.zst') { res.writeHead(200, { 'content-type': 'text/html' }); res.end('<!doctype html>'); return; }
  try { const b = readFileSync(join(dir, name)); res.writeHead(200); res.end(b); } catch { res.writeHead(404); res.end(); }
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const base = `http://127.0.0.1:${server.address().port}`;

let failed = 0;
const check = (name, cond) => { console.log(`${cond ? 'ok  ' : 'FAIL'} ${name}`); if (!cond) failed++; };
const same = (b) => { const u = new Uint8Array(b); return u.length === body.length && u.every((v, i) => v === body[i]); };
for (const [n, want] of [['z', ['/z.wasm.zst']], ['plain', ['/plain.wasm.zst', '/plain.wasm']], ['fallback', ['/fallback.wasm.zst', '/fallback.wasm']]]) {
  asked.length = 0;
  const got = await moduleBytes(`${base}/${n}.wasm`);
  check(`${n}: the same bytes`, same(got));
  check(`${n}: asked for ${want.join(', ')}`, JSON.stringify(asked) === JSON.stringify(want));
}
server.close();
rmSync(dir, { recursive: true, force: true });
console.log(failed ? `hostwl-zst: ${failed} failed` : 'hostwl-zst ok');
process.exit(failed ? 1 : 0);
