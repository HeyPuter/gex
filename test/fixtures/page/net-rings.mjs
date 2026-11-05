/**
 * Does a Worker read what a stream receives from its ring, in order, with
 * no call to the page?
 *
 * net-bridge.mjs gives each open stream a receive ring in shared memory:
 * net.js's Sockets writes what arrives into it (keeping what does not
 * fit, and moving that in as the ring empties), and a Worker's NetProxy
 * answers status, poll, recv and peek from it. This runs a Sockets over a
 * stand-in network in this thread (the page) and a NetProxy in a worker
 * thread:
 *
 *   - 3 MiB arrive in uneven chunks, faster than the Worker reads: it gets
 *     every byte in order, and the page is asked only to refill the ring
 *     (never for status, poll, recv or peek);
 *   - a peek leaves the bytes for the recv after it;
 *   - once the far end closes and the ring is drained, recv says 0 and the
 *     stream polls ready;
 *   - a stream opened with every ring taken still works through the page;
 *   - a stream closed, its ring given to a new one, reads as closed;
 *   - the rings' memory is none before the first stream, and grows by a
 *     few rings at a time.
 *
 * Usage: node test/fixtures/page/net-rings.mjs
 */
import { Worker, isMainThread, workerData, parentPort } from 'node:worker_threads';
import { register } from 'node:module';
import { pathToFileURL as gexPathToFileURL } from 'node:url';
import { join as gexJoin } from 'node:path';
// gex: the checkout these fixtures are in, or GEX_ROOT.
const GEX_ROOT = process.env.GEX_ROOT || gexJoin(import.meta.dirname, '..', '..', '..');
const gexUrl = (p) => gexPathToFileURL(gexJoin(GEX_ROOT, p)).href;

const TOTAL = 3 << 20;
const byte = (i) => (i * 131 + (i >>> 11)) & 255;

if (isMainThread) {
  const here = new URL(gexUrl('hosts/browser/js/')).href;
  // net.js's imports: gex's own modules.
  register(`data:text/javascript,${encodeURIComponent(`
    export async function resolve(spec, ctx, next) {
      if (spec.startsWith('@gex/')) return { url: ${JSON.stringify(here)} + spec.slice(5), shortCircuit: true };
      return next(spec, ctx);
    }`)}`);
  const { Sockets } = await import(gexUrl('page/src/net.js'));
  const { makeNetChannel, serveNet } = await import(gexUrl('hosts/browser/js/net-bridge.mjs'));
  // A stand-in network: each connection's far end, driven from here.
  const net = new Sockets({
    connect(host, port) {
      let c;
      const s = { host, port, sent: [], onopen() {}, onmessage(b) { c.enqueue(b); }, onclose() { c.close(); } };
      (globalThis.fakeStreams ??= []).push(s);
      return {
        readable: new ReadableStream({ start: (ctl) => { c = ctl; }, cancel: () => { s.closed = true; } }),
        writable: new WritableStream({ write: (b) => { s.sent.push(b); }, close: () => { s.closed = true; } }),
      };
    },
  });
  const asked = { status: 0, recv: 0, peek: 0, available: 0, refill: 0 };
  for (const k of Object.keys(asked)) {
    const f = net[k].bind(net);
    net[k] = (...a) => { asked[k]++; return f(...a); };
  }
  const channel = makeNetChannel();
  const before = channel.rxData.byteLength;
  serveNet(net, channel);
  const w = new Worker(new URL(import.meta.url), { workerData: { channel, total: TOTAL } });
  let bad = 0;
  const check = (what, ok, more = '') => {
    if (!ok) bad++;
    console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}${ok ? '' : ` ${more}`}`);
  };
  const stream = () => new Promise((resolve) => {
    const t = setInterval(() => {
      const s = globalThis.fakeStreams?.shift();
      if (s) { clearInterval(t); resolve(s); }
    }, 1);
  });
  const turn = () => new Promise((r) => setTimeout(r, 0));
  w.on('error', (e) => { console.log(`FAIL worker: ${e.stack ?? e}`); process.exit(1); });
  const said = new Promise((resolve) => w.on('message', (m) => { if (m.done) resolve(m); }));
  // The long stream.
  const s = await stream();
  const first = channel.rxData.byteLength;
  s.onopen();
  for (let at = 0, k = 0; at < TOTAL; k++) {
    const n = Math.min(TOTAL - at, 1 + ((k * 7919) % 90000));
    const b = new Uint8Array(n);
    for (let i = 0; i < n; i++) b[i] = byte(at + i);
    s.onmessage(b);
    at += n;
    if (k % 8 === 0) await turn();
  }
  s.onclose();
  // A stream once its ring is someone else's.
  const old = await stream();
  old.onopen();
  // Every ring taken, then one more stream.
  const extra = await stream();
  extra.onopen();
  extra.onmessage(new Uint8Array([1, 2, 3]));
  extra.onclose();
  const r = await said;
  check('no ring bytes before a stream, a few rings at the first', before === 0 && first === 4 << 18, `(${before}, then ${first})`);
  check('every byte, in order', r.got === TOTAL && r.wrong === -1, `(${r.got} of ${TOTAL}, first wrong at ${r.wrong})`);
  check('a peek leaves the bytes', r.peekOk);
  check('drained and closed: recv 0, polls ready', r.endRecv === 0 && r.endAvail === true, `(recv ${r.endRecv}, ready ${r.endAvail})`);
  // Besides refills: the stream without a ring's reads, and the closed
  // stream's status and recv (it has no ring any more).
  check('the page asked to refill only', asked.status + asked.recv + asked.peek + asked.available === r.ringless + 2 && asked.refill > 0,
    `(${JSON.stringify(asked)}, ${r.ringless} for the stream without a ring)`);
  check('a stream with no ring left goes through the page', r.extra === '1,2,3' && r.extraEnd === 0, `(${r.extra}, then ${r.extraEnd})`);
  check('a closed stream whose ring went to another reads as closed', r.reused === 2 && r.reusedRecv === 0, `(status ${r.reused}, recv ${r.reusedRecv})`);
  console.log(bad ? `${bad} failed` : 'streams read from their rings, the page asked only to refill');
  process.exit(bad ? 1 : 0);
} else {
  const { NetProxy } = await import(gexUrl('hosts/browser/js/net-bridge.mjs'));
  const p = new NetProxy(workerData.channel, 2);
  const out = { done: true };
  // A timer beside the wait: Node lets a thread whose only pending work is
  // an Atomics.waitAsync end.
  const wait = async () => {
    const seq = p.activitySeq();
    await Promise.race([p.activityWait(seq, 20), new Promise((r) => setTimeout(r, 20))]);
  };
  const id = p.open();
  p.connect(id, 'example.org', 80);
  while (p.status(id) === 0) await wait();
  let got = 0, wrong = -1, peeked = false;
  out.peekOk = false;
  const buf = new Uint8Array(70000);
  for (;;) {
    if (!peeked && got > 1 << 20) {
      while (!p.available(id)) await wait();
      const pk = new Uint8Array(10);
      const n = p.peek(id, pk);
      const b2 = new Uint8Array(10);
      const m = p.recv(id, b2);
      out.peekOk = n > 0 && m >= n && pk.subarray(0, n).every((v, i) => v === b2[i]);
      for (let i = 0; i < m; i++) if (wrong < 0 && b2[i] !== byte(got + i)) wrong = got + i;
      got += m;
      peeked = true;
      continue;
    }
    const n = p.recv(id, buf.subarray(0, 1 + (got % 69999)));
    if (n === 0) break;
    if (n < 0) { await wait(); continue; }
    for (let i = 0; i < n; i++) if (wrong < 0 && buf[i] !== byte(got + i)) wrong = got + i;
    got += n;
  }
  out.got = got; out.wrong = wrong;
  out.endRecv = p.recv(id, buf);
  out.endAvail = p.available(id);
  p.close(id);
  // Its ring goes to the next stream; the old id then reads as closed.
  const id2 = p.open();
  p.connect(id2, 'example.org', 80);
  out.reused = p.status(id);
  out.reusedRecv = p.recv(id, buf);
  // Every other ring taken, then one stream more, which has none.
  const held = [];
  for (let i = 0; i < 63; i++) held.push(p.open());
  const id3 = p.open();
  p.connect(id3, 'example.org', 80);
  let ringless = 0;
  const parts = [];
  for (;;) {
    ringless++;
    const n = p.recv(id3, buf);
    if (n === 0) break;
    if (n < 0) { await wait(); continue; }
    parts.push(...buf.subarray(0, n));
  }
  out.extra = parts.join(',');
  ringless++;
  out.extraEnd = p.recv(id3, buf);
  out.ringless = ringless;
  parentPort.postMessage(out);
}
