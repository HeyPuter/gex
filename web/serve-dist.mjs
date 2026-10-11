// Serve the built site, cross-origin isolated, on a fixed port.
//
//     node web/serve-dist.mjs [port] [--sp-port N]
//
// `--sp-port N` also serves web/dist-sp, the single-thread build's dist
// (web/build-all.sh --sp; docs/single-thread.md), on N without the isolation
// headers: MP on [port], SP on N, side by side. `GEX_NO_COI=1` drops the
// headers on [port] too.
//
// Not `vite dev`/`preview`: the dev server has served stale JavaScript against
// a freshly built shim, so a measurement against it means nothing. `web/dist`
// is what the harness runs and a deploy copies. The two isolation headers are
// set here, not assumed of the server -- without them there is no
// SharedArrayBuffer, and threads and the global kernel silently vanish.
//
// Open the URL it prints, choose a demo, and read the per-process console lines
// `[gex] pid N: lived Xms (guest Yms)` -- guest-side timing reads zero (#134).

import { createServer } from 'node:http';
import { appendFileSync } from 'node:fs';
import { readFile, stat } from 'node:fs/promises';
import { extname, join, normalize } from 'node:path';
import { fileURLToPath } from 'node:url';

const web = fileURLToPath(new URL('.', import.meta.url));
const root = join(web, 'dist');
const spRoot = join(web, 'dist-sp');
const args = process.argv.slice(2);
const spAt = args.indexOf('--sp-port');
const spPort = spAt >= 0 ? Number(args.splice(spAt, 2)[1]) : 0;
const port = Number(args[0] ?? 8088);

const TYPES = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json',
  '.wasm': 'application/wasm',
  '.tar': 'application/x-tar',
  '.meta': 'application/octet-stream',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.ico': 'image/x-icon',
};

const serve = (root, coi) => async (req, res) => {
  const url = new URL(req.url, 'http://x');
  // `normalize` and then reject anything that climbs out: this serves a
  // directory, not the filesystem.
  const rel = normalize(decodeURIComponent(url.pathname)).replace(/^(\.\.[/\\])+/, '');
  let path = join(root, rel);
  try {
    if ((await stat(path)).isDirectory()) path = join(path, 'index.html');
  } catch {
    // A missing path is answered below, once, rather than here and there.
  }
  const headers = {
    // What buys `SharedArrayBuffer`. See the note at the top. `GEX_NO_COI=1`
    // leaves them off, as any static host would: the page then runs the
    // single-thread build (docs/single-thread.md).
    ...(!coi ? {} : {
      'Cross-Origin-Opener-Policy': 'same-origin',
      'Cross-Origin-Embedder-Policy': 'require-corp',
    }),
    // The closure is hundreds of MB and not content-hashed, so a cache would
    // serve yesterday's libc against today's shim.
    'Cache-Control': 'no-store',
  };
  try {
    const body = await readFile(path);
    res.writeHead(200, {
      ...headers,
      'content-type': TYPES[extname(path)] ?? 'application/octet-stream',
      'content-length': body.length,
    });
    res.end(body);
    // GEX_SERVE_LOG=FILE: one line per file served, `<dist> <path> <bytes>`,
    // to account for what a page fetches.
    if (process.env.GEX_SERVE_LOG) appendFileSync(process.env.GEX_SERVE_LOG, `${root} ${rel} ${body.length}\n`);
  } catch {
    res.writeHead(404, { ...headers, 'content-type': 'text/plain' }).end(`no ${rel}\n`);
  }
};

const coi = !process.env.GEX_NO_COI;
createServer(serve(root, coi)).listen(port, '0.0.0.0', () => {
  console.log(`serving ${root}`);
  console.log(`  http://127.0.0.1:${port}/  ${coi ? 'multi-process (cross-origin isolated, SharedArrayBuffer)' : 'single-thread (no COOP/COEP)'}`);
  if (!spPort) {
    console.log('');
    console.log('  open the console for `[gex] pid N: lived Xms (guest Yms)`');
  }
});
if (spPort) {
  createServer(serve(spRoot, false)).listen(spPort, '0.0.0.0', () => {
    console.log(`serving ${spRoot}`);
    console.log(`  http://127.0.0.1:${spPort}/  single-thread (no COOP/COEP)`);
  });
}
