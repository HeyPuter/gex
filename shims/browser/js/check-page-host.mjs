/**
 * Does a page-served host (page-host.mjs) answer a guest's HTTP the way a
 * web server would?
 *
 * Through NodeNetwork, as run-node's GEX_PAGE_HOSTS gives it: a stream to
 * the named host, three pipelined requests on it (a GET, a HEAD, a GET of a
 * file there is none of), each answered in order; a path that climbs out of
 * the directory is not served; a POST gets 405 and the connection ends; and
 * `Connection: close` ends it after the answer.
 *
 * Usage: node check-page-host.mjs
 */
import { mkdtempSync, writeFileSync, rmSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { NodeNetwork } from './net-node.mjs';

const dir = mkdtempSync(join(tmpdir(), 'gex-page-host-'));
mkdirSync(join(dir, 'pool'));
writeFileSync(join(dir, 'pool', 'a.txt'), 'twelve bytes');
writeFileSync(join(dir, '..', `${dir.split('/').pop()}-outside`), 'not served');
const net = new NodeNetwork({ pageHosts: new Map([['files.test', `${pathToFileURL(dir).href}/`]]) });

/** Send `req` on a new stream, and read until the stream ends or `until`
 *  says the answer is whole. */
async function exchange(req, until) {
  const id = net.open();
  net.connect(id, 'FILES.test', 80);
  if (net.status(id) !== 1) throw new Error(`not open: ${net.status(id)}`);
  net.send(id, new TextEncoder().encode(req));
  let got = '';
  const buf = new Uint8Array(65536);
  const t0 = Date.now();
  for (;;) {
    const n = net.recv(id, buf);
    if (n > 0) got += new TextDecoder().decode(buf.subarray(0, n));
    if (n === 0) return { got, ended: true };
    if (until?.(got)) return { got, ended: false, id };
    if (Date.now() - t0 > 5000) throw new Error(`no answer: ${JSON.stringify(got)}`);
    await new Promise((r) => setTimeout(r, 5));
  }
}

let failed = 0;
const check = (ok, what, got) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) {
    failed++;
    console.log(JSON.stringify(got));
  }
};

const three = await exchange(
  'GET /pool/a.txt HTTP/1.1\r\nHost: files.test\r\n\r\n'
  + 'HEAD /pool/a.txt HTTP/1.1\r\nHost: files.test\r\n\r\n'
  + 'GET /pool/none HTTP/1.1\r\nHost: files.test\r\n\r\n',
  (g) => (g.match(/HTTP\/1\.1 /g) ?? []).length === 3 && g.endsWith('\r\n\r\n'),
);
const answers = three.got.split(/(?=HTTP\/1\.1 )/);
check(answers.length === 3, 'three pipelined requests, three answers', three.got);
check(/^HTTP\/1\.1 200 [^]*Content-Length: 12\r\n/.test(answers[0] ?? '') && (answers[0] ?? '').endsWith('\r\n\r\ntwelve bytes'), 'GET: the file', answers[0]);
check(/^HTTP\/1\.1 200 [^]*Content-Length: 12\r\n/.test(answers[1] ?? '') && (answers[1] ?? '').endsWith('\r\n\r\n'), 'HEAD: its length, no body', answers[1]);
check(/^HTTP\/1\.1 404 /.test(answers[2] ?? ''), 'a missing file: 404', answers[2]);
check(!three.ended, 'keep-alive: the stream stays open', three);
if (three.id) net.close(three.id);

const out = await exchange(`GET /../${dir.split('/').pop()}-outside HTTP/1.1\r\nHost: files.test\r\nConnection: close\r\n\r\n`);
check(/^HTTP\/1\.1 404 /.test(out.got) && out.ended, 'a path out of the directory: 404', out.got);

const post = await exchange('POST /pool/a.txt HTTP/1.1\r\nHost: files.test\r\nContent-Length: 2\r\n\r\nhi'
  + 'GET /pool/a.txt HTTP/1.1\r\nHost: files.test\r\n\r\n');
check(/^HTTP\/1\.1 405 [^]*Allow: GET, HEAD/.test(post.got) && post.ended && !/200/.test(post.got),
  'POST: 405, and the connection ends, the request after it not answered', post.got);

const part = await exchange('GET /pool/a.txt HTTP/1.1\r\nHost: files.test\r\nRange: bytes=4-6\r\nConnection: close\r\n\r\n');
check(/^HTTP\/1\.1 206 [^]*Content-Range: bytes 4-6\/12\r\n[^]*Content-Length: 3\r\n/.test(part.got) && part.got.endsWith('\r\n\r\nve '),
  'a range: 206, those bytes', part.got);

const ws = await exchange('GET /pool/a.txt HTTP/1.1\r\nHost: files.test\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n');
check(/^HTTP\/1\.1 405 /.test(ws.got) && ws.ended, 'a WebSocket upgrade: 405', ws.got);

const close = await exchange('GET /pool/a.txt HTTP/1.1\r\nHost: files.test\r\nConnection: close\r\n\r\n');
check(/twelve bytes$/.test(close.got) && /Connection: close/.test(close.got) && close.ended, 'Connection: close ends it after the answer', close.got);

const old = await exchange('GET /pool/a.txt HTTP/1.0\r\n\r\n');
check(/twelve bytes$/.test(old.got) && old.ended, 'HTTP/1.0: answered, then closed', old.got);

rmSync(dir, { recursive: true, force: true });
rmSync(join(dir, '..', `${dir.split('/').pop()}-outside`), { force: true });
console.log(failed ? `${failed} failed` : 'all passed');
process.exit(failed ? 1 : 0);
