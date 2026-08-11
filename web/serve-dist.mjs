// Serve the built site, cross-origin isolated, on a fixed port.
//
// Not `vite dev` and not `vite preview`. The dev server compiles the sources
// again on its own terms and has served stale JavaScript against a freshly
// built shim more than once -- a measurement taken against it is a
// measurement of something nobody shipped. `web/dist` is what the test
// harness runs and what a deploy copies, so it is what a number should come
// from.
//
// The two headers are not optional: without them the page has no
// `SharedArrayBuffer`, the guest cannot have a shared memory, and threads and
// the global kernel are gone. That failure is quiet -- the page loads and
// then behaves like a different system -- so they are set here rather than
// assumed of whatever is serving.
//
//     node web/serve-dist.mjs [port]
//
// Then open the URL it prints, choose a demo, and read the per-process lines
// in the browser console: `[gex] pid N: lived Xms (guest Yms)`. Guest-side
// timing does not work -- see #134, `date +%s%N` around a 370ms command
// reads zero -- so the console is where the numbers are.

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
    // The closure is hundreds of megabytes and none of it is content-hashed,
    // so a browser that caches it will happily serve yesterday's libc against
    // today's shim. That combination has already cost an afternoon.
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
