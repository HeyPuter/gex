// A network (sys.network) that answers one name itself: a program's
// connection to `gex-repo` port 80 gets a small HTTP/1.1 server that fetches
// each path it is asked for under a base URL of the page's (a directory the
// page serves, say debdemo's gex-repo.tar unpacked), and every other
// connection goes to the network it was given (a Wisp, or none). apt's
// `deb [trusted=yes arch=musl-linux-wasm32,arm64] http://gex-repo/ trixie main`
// then installs from the page's own origin.
//
//   import { RepoNetwork } from './repo-network.mjs';
//   sys.network = new RepoNetwork({ base: new URL('repo/', location.href), fallback: new Wisp(url) });
//
// The server: GET and HEAD; requests sent one after another on a
// connection (apt pipelines them) answered in their order; If-Modified-Since,
// If-None-Match, Range and If-Range passed on to the fetch, and its 304 or
// 206 passed back; Content-Length when the fetch has one for the bytes it
// gives (not when it decoded a Content-Encoding), chunked otherwise. An HTML
// answer for a path not ending in .html is a 404: a Debian repository serves
// no HTML, and a static server's fallback page (an SPA's or a dev server's
// index.html for every unknown path) in place of a missing InRelease would
// make apt fail its signature check rather than fall back to Release.

const enc = new TextEncoder();
const dec = new TextDecoder('latin1');
const PASSED_ON = ['if-modified-since', 'if-none-match', 'range', 'if-range'];
const PASSED_BACK = ['last-modified', 'etag', 'content-range', 'accept-ranges', 'content-type'];
const REASONS = { 200: 'OK', 206: 'Partial Content', 304: 'Not Modified', 400: 'Bad Request', 404: 'Not Found', 405: 'Method Not Allowed', 416: 'Range Not Satisfiable', 502: 'Bad Gateway' };

export class RepoNetwork {
  /** `base`: the URL each path is fetched under (a string or URL, ending in
   *  `/`). `host`, `port`: the name it answers (`gex-repo`, 80).
   *  `fallback`: the network every other connection goes to, or null (they
   *  are refused). */
  constructor({ base, host = 'gex-repo', port = 80, fallback = null, fetch: f = globalThis.fetch.bind(globalThis) }) {
    this.base = new URL(base);
    this.host = host;
    this.port = port;
    this.fallback = fallback;
    this.fetch = f;
  }

  connect(host, port) {
    if (host === this.host && port === this.port) return this.#serve();
    if (!this.fallback) throw new Error(`${host}:${port}: no network`);
    return this.fallback.connect(host, port);
  }

  datagram(host, port) {
    if (!this.fallback?.datagram) throw new Error(`${host}:${port}: no UDP`);
    return this.fallback.datagram(host, port);
  }

  /** One connection: its requests parsed as the program's bytes come, each
   *  fetched at once, and the answers written in the requests' order. */
  #serve() {
    let out, room = null;
    const readable = new ReadableStream({
      start: (c) => { out = c; },
      pull: () => { room?.(); room = null; },
      cancel: () => { closed = true; room?.(); },
    }, { highWaterMark: 4 });
    let closed = false;
    const send = async (bytes) => {
      if (closed) return;
      out.enqueue(bytes);
      while (!closed && out.desiredSize <= 0) await new Promise((r) => { room = r; });
    };
    const end = () => { if (!closed) { closed = true; out.close(); } };
    let buf = new Uint8Array(0);
    let answered = Promise.resolve();
    let ending = false;
    const take = () => {
      for (;;) {
        const at = headEnd(buf);
        if (at < 0) return;
        const req = parse(dec.decode(buf.subarray(0, at)));
        buf = buf.subarray(at + 4);
        const skip = req ? Number(req.headers['content-length'] || 0) : 0;
        if (skip > buf.length) { buf = new Uint8Array(0); return; } // a body (not asked for here): the rest is dropped
        buf = buf.subarray(skip);
        const reply = req ? this.#answer(req) : Promise.resolve({ status: 400, headers: new Headers(), body: null });
        const last = !req || req.close;
        answered = answered.then(async () => { if (!ending) await this.#write(send, req, await reply); if (last) { ending = true; end(); } })
          .catch(() => { ending = true; end(); });
        if (last) return;
      }
    };
    const writable = new WritableStream({
      write: (b) => {
        const n = new Uint8Array(buf.length + b.length);
        n.set(buf); n.set(b, buf.length);
        buf = n;
        take();
      },
      close: () => { answered.then(end); },
      abort: () => { closed = true; },
    });
    return { readable, writable };
  }

  /** A request's fetch, started now. */
  async #answer(req) {
    if (req.method !== 'GET' && req.method !== 'HEAD') return { status: 405, headers: new Headers(), body: null };
    const path = req.target.replace(/^[a-z]+:\/\/[^/]*/i, '').replace(/^\/+/, '');
    const url = new URL(path, this.base);
    if (!url.href.startsWith(this.base.href)) return { status: 404, headers: new Headers(), body: null };
    const headers = {};
    for (const h of PASSED_ON) if (req.headers[h]) headers[h] = req.headers[h];
    try {
      const r = await this.fetch(url, { method: req.method, headers, cache: 'no-store' });
      if (r.ok && /^text\/html\b/i.test(r.headers.get('content-type') || '') && !/\.html?$/i.test(url.pathname)) {
        r.body?.cancel().catch(() => {});
        return { status: 404, headers: new Headers(), body: null };
      }
      return { status: r.status, headers: r.headers, body: req.method === 'HEAD' ? null : r.body };
    } catch {
      return { status: 502, headers: new Headers(), body: null };
    }
  }

  async #write(send, req, { status, headers, body }) {
    const lines = [`HTTP/1.1 ${status} ${REASONS[status] || 'Status'}`];
    for (const h of PASSED_BACK) if (headers.get(h)) lines.push(`${h}: ${headers.get(h)}`);
    const bodyless = status === 304 || status === 204 || status < 200;
    // The fetch's length is of the bytes it gives only when it decoded nothing.
    const length = headers.get('content-encoding') ? null : headers.get('content-length');
    const chunked = !bodyless && body && length == null;
    if (bodyless) { /* no body, no framing */ }
    else if (chunked) lines.push('transfer-encoding: chunked');
    else lines.push(`content-length: ${body ? length : (req?.method === 'HEAD' && length != null ? length : 0)}`);
    if (!req || req.close) lines.push('connection: close');
    await send(enc.encode(lines.join('\r\n') + '\r\n\r\n'));
    if (bodyless || !body) { body?.cancel().catch(() => {}); return; }
    const r = body.getReader();
    for (;;) {
      const { value, done } = await r.read();
      if (done) break;
      if (!value.length) continue;
      if (chunked) await send(enc.encode(value.length.toString(16) + '\r\n'));
      await send(value);
      if (chunked) await send(enc.encode('\r\n'));
    }
    if (chunked) await send(enc.encode('0\r\n\r\n'));
  }
}

/** Where a request's head ends (its `\r\n\r\n`), or -1. */
function headEnd(b) {
  for (let i = 0; i + 3 < b.length; i++)
    if (b[i] === 13 && b[i + 1] === 10 && b[i + 2] === 13 && b[i + 3] === 10) return i;
  return -1;
}

/** A request's head: method, target, headers (lower-case names), and
 *  whether the connection ends after it. Null if it is not HTTP/1.x. */
function parse(text) {
  const [line, ...rest] = text.split('\r\n');
  const m = /^([A-Z]+) (\S+) HTTP\/1\.([01])$/.exec(line);
  if (!m) return null;
  const headers = {};
  for (const l of rest) {
    const i = l.indexOf(':');
    if (i > 0) headers[l.slice(0, i).trim().toLowerCase()] = l.slice(i + 1).trim();
  }
  const conn = (headers.connection || '').toLowerCase();
  return { method: m[1], target: m[2], headers, close: m[3] === '0' ? conn !== 'keep-alive' : conn === 'close' };
}
