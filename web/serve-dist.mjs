// Serve the built site, cross-origin isolated, on a fixed port.
//
//     node web/serve-dist.mjs [port]
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
import { readFile, stat } from 'node:fs/promises';
import { extname, join, normalize } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(fileURLToPath(new URL('.', import.meta.url)), 'dist');
const port = Number(process.argv[2] ?? 8088);

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

const server = createServer(async (req, res) => {
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
    // What buys `SharedArrayBuffer`. See the note at the top.
    'Cross-Origin-Opener-Policy': 'same-origin',
    'Cross-Origin-Embedder-Policy': 'require-corp',
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
  } catch {
    res.writeHead(404, { ...headers, 'content-type': 'text/plain' }).end(`no ${rel}\n`);
  }
});

server.listen(port, '0.0.0.0', () => {
  console.log(`serving ${root}`);
  console.log(`  http://127.0.0.1:${port}/`);
  console.log('');
  console.log('  cross-origin isolated, so SharedArrayBuffer works');
  console.log('  open the console for `[gex] pid N: lived Xms (guest Yms)`');
});
