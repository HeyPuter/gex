// apt from a repository the page serves, through the network: the example
// RepoNetwork (page/runtime/examples/repo-network.mjs) answers the guest's
// connections to gex-repo:80 by fetching under a base URL, here a small
// HTTP server of the test's over the image's repository (GEX_TEST_SLIM).
// First the server alone: requests sent together answered in their order,
// If-Modified-Since's 304, chunked where the fetch decoded a gzip body; then
// the image's `apt-get update && apt-get install` with the gex.list it
// carries, and more updates, which fetch nothing new (a 304).
import { createServer } from 'node:http';
import { createReadStream, readFileSync, statSync } from 'node:fs';
import { join, normalize } from 'node:path';
import { gzipSync } from 'node:zlib';
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';
import { RepoNetwork } from '../../page/runtime/examples/repo-network.mjs';

export const meta = { hosts: ['node'], images: ['slim'], timeout: 600 };

const PKG = 'xcvt';
const enc = new TextEncoder();
const dec = new TextDecoder('latin1');

/** The repository as a static server: Last-Modified and If-Modified-Since,
 *  and /gz/PATH as PATH gzip-encoded with no length. `dated.at` (seconds),
 *  once set: every file's time, as gex-repo.tar unpacked gives them (the
 *  build's time, the image's lists' too). `dated.spa`, once set: an unknown
 *  path answered 200 with an index.html, as an SPA's or a dev server's
 *  static server does. */
function serve(root, log, dated = {}) {
  const server = createServer((req, res) => {
    let path = decodeURIComponent(new URL(req.url, 'http://x').pathname);
    const gz = path.startsWith('/gz/');
    if (gz) path = path.slice(3);
    const file = join(root, normalize(path).replace(/^(\.\.[/\\])+/, ''));
    let st;
    try { st = statSync(file); } catch { st = null; }
    if (!st?.isFile() && dated.spa) {
      log.push(`html ${path}`);
      res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' }).end('<!doctype html><title>app</title><div id="root"></div>');
      return;
    }
    if (!st?.isFile()) { log.push(`404 ${path}`); res.writeHead(404).end(); return; }
    const lm = new Date((dated.at ?? Math.floor(st.mtimeMs / 1000)) * 1000);
    const ims = req.headers['if-modified-since'];
    if (ims && new Date(ims) >= lm) { log.push(`304 ${path}`); res.writeHead(304, { 'last-modified': lm.toUTCString() }).end(); return; }
    log.push(`200 ${path}`);
    if (gz) {
      res.writeHead(200, { 'content-encoding': 'gzip', 'last-modified': lm.toUTCString() }).end(gzipSync(readFileSync(file)));
      return;
    }
    res.writeHead(200, { 'content-length': st.size, 'last-modified': lm.toUTCString() });
    createReadStream(file).pipe(res);
  });
  return new Promise((r) => server.listen(0, '127.0.0.1', () => r(server)));
}

/** Everything the server sends on one connection, until it closes it. */
async function exchange(net, text) {
  const d = await net.connect('gex-repo', 80);
  const w = d.writable.getWriter();
  await w.write(enc.encode(text));
  const parts = [];
  for (const r = d.readable.getReader(); ;) {
    const { value, done } = await r.read();
    if (done) break;
    parts.push(value);
  }
  return dec.decode(Buffer.concat(parts));
}

export default async function (t) {
  const { say, check, eq, timeout } = t;
  const repo = join(t.workDir, 'slim', 'repo');
  const log = [];
  const dated = {};
  const server = await serve(repo, log, dated);
  const base = `http://127.0.0.1:${server.address().port}/`;
  try {
    const refused = [];
    const net = new RepoNetwork({ base, fallback: { connect: (h, p) => { refused.push(`${h}:${p}`); throw new Error('offline'); } } });
    check('another host goes to the fallback', await Promise.resolve().then(() => net.connect('deb.debian.org', 80)).then(() => false, () => refused[0] === 'deb.debian.org:80'));

    // Two requests in one write, the second closing: both answered, in order.
    const rel = 'dists/trixie/Release', pk = 'dists/trixie/main/binary-musl-linux-wasm32/Packages';
    const two = await exchange(net, `GET /${rel} HTTP/1.1\r\nHost: gex-repo\r\n\r\nHEAD /${pk} HTTP/1.1\r\nHost: gex-repo\r\nConnection: close\r\n\r\n`);
    const heads = [...two.matchAll(/^HTTP\/1\.1 (\d+)[^\r]*\r\n((?:[^\r]+\r\n)*)\r\n/gm)];
    eq('pipelined: two answers', heads.length, 2);
    check('pipelined: the Release first, with its length', /content-length: (\d+)/.exec(heads[0]?.[2] || '')?.[1] === String(statSync(join(repo, rel)).size), heads[0]?.[0]);
    check('pipelined: then the Packages HEAD, its length and no body', heads[1]?.[2].includes(`content-length: ${statSync(join(repo, pk)).size}`) && two.endsWith('\r\n\r\n'), heads[1]?.[0]);
    const lm = /last-modified: ([^\r]+)/.exec(heads[0]?.[2] || '')?.[1];
    check('Last-Modified passed back', !!lm, heads[0]?.[0]);

    // If-Modified-Since passed on, the 304 passed back with no body.
    const nm = await exchange(net, `GET /${rel} HTTP/1.1\r\nIf-Modified-Since: ${lm}\r\nConnection: close\r\n\r\n`);
    check('If-Modified-Since: 304, no body', /^HTTP\/1\.1 304 /.test(nm) && nm.endsWith('\r\n\r\n') && !/content-length|transfer-encoding/i.test(nm), nm);

    // A gzip-encoded answer the fetch decoded: chunked, the decoded bytes.
    const gz = await exchange(net, `GET /gz/${rel} HTTP/1.1\r\nConnection: close\r\n\r\n`);
    const head = gz.slice(0, gz.indexOf('\r\n\r\n')), body = gz.slice(gz.indexOf('\r\n\r\n') + 4);
    check('Content-Encoding: chunked', /transfer-encoding: chunked/i.test(head) && !/content-length/i.test(head), head);
    let plain = '', rest = body;
    for (;;) {
      const i = rest.indexOf('\r\n');
      const n = parseInt(rest.slice(0, i), 16);
      if (!n) break;
      plain += rest.slice(i + 2, i + 2 + n);
      rest = rest.slice(i + 2 + n + 2);
    }
    eq('chunked: the decoded bytes', plain, dec.decode(readFileSync(join(repo, rel))));

    const nf = await exchange(net, 'GET /no/such HTTP/1.1\r\nConnection: close\r\n\r\n');
    check('a missing path: 404', /^HTTP\/1\.1 404 /.test(nf), nf);
    const up = await exchange(net, 'GET /../../etc/passwd HTTP/1.1\r\nConnection: close\r\n\r\n');
    check('nothing above the base', !/root:/.test(up), up.slice(0, 200));

    // The image: its gex.list names http://gex-repo/, its lists know the repository.
    const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
    const fs = new MemoryFilesystem(sys);
    fs.addTar(t.work('slim/debian-slim.tar.zst'));
    sys.network = net;
    await timeout(sys.boot(), 180000, 'boot');
    const env = { PATH: '/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin', HOME: '/root', DEBIAN_FRONTEND: 'noninteractive' };
    const run = async (argv, ms = 300000) => {
      const t0 = performance.now();
      const r = await timeout(sys.run(argv, { env }), ms, argv.join(' '));
      say(`${argv.join(' ')}: exit ${r.code} in ${(performance.now() - t0).toFixed(0)} ms`);
      if (r.code) say(`  ${(r.stderr || r.stdout).slice(-800)}`);
      return r;
    };
    const list = (await run(['cat', '/etc/apt/sources.list.d/gex.list'])).stdout;
    check('the image\'s gex.list names http://gex-repo/', /^deb \[trusted=yes arch=musl-linux-wasm32,arm64\] http:\/\/gex-repo\/ trixie main$/m.test(list), list);
    // Offline: Debian's archive is not reached here.
    await run(['rm', '-f', '/etc/apt/sources.list.d/debian.sources']);
    eq(`apt-cache policy ${PKG} before an update`, /Candidate: (?!\(none\))/.test((await run(['apt-cache', 'policy', PKG])).stdout), true);
    log.length = 0;
    eq('apt-get update', (await run(['apt-get', 'update'])).code, 0);
    check('apt-get update fetched from the repository', log.some((l) => l.includes('/dists/trixie/')), log.join('\n'));
    eq(`apt-get install ${PKG}`, (await run(['apt-get', 'install', '-y', PKG])).code, 0);
    check(`${PKG} came from the repository`, log.some((l) => l.startsWith('200 /pool/') && l.includes(`/${PKG}_`)), log.join('\n'));
    eq(`${PKG} runs`, (await run(['cvt', '1920', '1080'])).stdout.includes('Modeline'), true);
    // Again: the indexes are unchanged (the Release names the same
    // Packages, which are not fetched). apt asks for the Release
    // If-Modified-Since its list's time, the image's: this test's
    // repository files are newer (200), the tar's unpacked are not (304).
    log.length = 0;
    eq('apt-get update again', (await run(['apt-get', 'update'])).code, 0);
    check('the second update fetches no Packages', !log.some((l) => l.startsWith('200 ') && /\/Packages/.test(l)), log.join('\n'));
    // The repository as gex-repo.tar unpacked serves it: its files dated as
    // the build, which dated the image's lists the same.
    dated.at = Number((await run(['stat', '-c', '%Y', '/etc/apt/sources.list.d/gex.list'])).stdout.trim());
    log.length = 0;
    eq('apt-get update a third time', (await run(['apt-get', 'update'])).code, 0);
    check('the third update is conditional: the Release 304', log.includes('304 /dists/trixie/Release'), log.join('\n'));
    say(log.join(', '));
    // A static server whose unknown paths give a 200 index.html: the missing
    // InRelease is still a 404 to apt, which then takes the Release.
    dated.spa = true;
    const spa = await exchange(net, 'GET /dists/trixie/InRelease HTTP/1.1\r\nConnection: close\r\n\r\n');
    check('an HTML fallback for a repository path: 404', /^HTTP\/1\.1 404 /.test(spa) && !/<html|doctype/i.test(spa), spa.slice(0, 300));
    log.length = 0;
    eq('apt-get update behind an HTML fallback', (await run(['apt-get', 'update'])).code, 0);
    check('the fallback answered InRelease, and apt went on to the Release', log.includes('html /dists/trixie/InRelease') && log.some((l) => / \/dists\/trixie\/Release$/.test(l)), log.join('\n'));
    await sys.shutdown();
  } finally {
    server.close();
  }
}
