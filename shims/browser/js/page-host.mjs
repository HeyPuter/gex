// Page-served hosts: a guest connection to one of a few named hosts is
// answered by the page itself, from files under a URL it can fetch (its own
// origin, say), instead of going out over the network. The guest speaks
// plain HTTP/1.1 to it; GET and HEAD, keep-alive and pipelined requests,
// each answered from `fetch` as its body arrives. An ejected page serves its
// guest any files this way (a package repository beside the page, which the
// network proxy could not reach).
//
// A stream here has the shape of a Wisp client's (`send`, `close`, and the
// `onopen`/`onmessage`/`onclose` its owner sets), so a network that holds
// Wisp streams holds these the same way.

/** `{host: base}` with each base a URL ending in a slash (relative ones
 *  against `relativeTo`), or null for none. */
export function pageHosts(map, relativeTo) {
  if (!map || typeof map !== 'object') return null;
  const out = new Map();
  for (const [host, base] of Object.entries(map)) {
    const u = new URL(String(base), relativeTo);
    if (!u.pathname.endsWith('/')) u.pathname += '/';
    out.set(host.toLowerCase(), u.href);
  }
  return out.size ? out : null;
}

const REASON = { 200: 'OK', 206: 'Partial Content', 304: 'Not Modified', 400: 'Bad Request', 404: 'Not Found', 405: 'Method Not Allowed', 416: 'Range Not Satisfiable', 502: 'Bad Gateway' };
/** What of the guest's request goes on to `fetch`. */
const FORWARD = ['range', 'if-range', 'if-modified-since', 'if-none-match', 'accept'];
/** What of the response comes back. */
const KEEP = ['content-type', 'last-modified', 'etag', 'content-range', 'accept-ranges', 'cache-control'];
const enc = new TextEncoder();

export class PageHostStream {
  /** Open from the start: there is no far end to wait for. */
  open = true;
  onopen = null;
  onmessage = null;
  onclose = null;
  onerror = null;
  #base;
  #fetch;
  #in = new Uint8Array(0);
  #queue = Promise.resolve();
  #closed = false;
  /** A request this does not answer has come: nothing after it is read. */
  #stopped = false;
  #reader = null;

  /** `base`: the URL (ending in a slash) a request's path is under;
   *  `fetchImpl`: `fetch`, or what stands in for it (a test's files). */
  constructor(base, fetchImpl = globalThis.fetch) {
    this.#base = base;
    this.#fetch = fetchImpl;
    queueMicrotask(() => this.onopen?.());
  }

  send(bytes) {
    if (this.#closed || this.#stopped) return;
    const all = new Uint8Array(this.#in.length + bytes.length);
    all.set(this.#in);
    all.set(bytes, this.#in.length);
    this.#in = all;
    // Every whole request head, in order: pipelined ones answered one
    // after another.
    for (;;) {
      const end = headEnd(this.#in);
      if (end < 0) break;
      const head = new TextDecoder().decode(this.#in.subarray(0, end));
      this.#in = this.#in.subarray(end + 4);
      const req = parse(head);
      this.#queue = this.#queue.then(() => this.#serve(req)).catch(() => this.#end());
      // A request with a body is not one this answers (no POST): the
      // connection ends after its 405, so whatever follows is not read.
      if (req.body || req.bad) {
        this.#stopped = true;
        break;
      }
    }
  }

  close() {
    this.#closed = true;
    this.#reader?.cancel().catch(() => {});
  }

  #emit(bytes) {
    if (!this.#closed && bytes.length) this.onmessage?.(bytes);
  }

  #end() {
    if (this.#closed) return;
    this.#closed = true;
    this.onclose?.();
  }

  #head(status, headers) {
    const lines = [`HTTP/1.1 ${status} ${REASON[status] ?? ''}`.trimEnd()];
    for (const [k, v] of headers) lines.push(`${k}: ${v}`);
    this.#emit(enc.encode(`${lines.join('\r\n')}\r\n\r\n`));
  }

  async #serve(req) {
    if (this.#closed) return;
    const allowed = (req.method === 'GET' || req.method === 'HEAD') && !req.body && !req.bad
      && !(req.headers.get('upgrade'));
    if (!allowed) {
      const status = req.bad ? 400 : 405;
      this.#head(status, [['Allow', 'GET, HEAD'], ['Content-Length', '0'], ['Connection', 'close']]);
      this.#end();
      return;
    }
    const keep = req.version === 'HTTP/1.1' && !/\bclose\b/i.test(req.headers.get('connection') ?? '');
    const conn = keep ? [] : [['Connection', 'close']];
    // The path, under the base and nowhere above it.
    const path = req.target.replace(/^[a-z]+:\/\/[^/]*/i, '').replace(/^\/+/, '');
    const url = new URL(path, this.#base);
    if (!url.href.startsWith(this.#base)) {
      this.#head(404, [['Content-Length', '0'], ...conn]);
      if (!keep) this.#end();
      return;
    }
    const headers = {};
    for (const h of FORWARD) if (req.headers.has(h)) headers[h] = req.headers.get(h);
    let r;
    try {
      r = await this.#fetch(url.href, { method: req.method, headers });
    } catch {
      this.#head(502, [['Content-Length', '0'], ...conn]);
      if (!keep) this.#end();
      return;
    }
    const out = [];
    for (const h of KEEP) if (r.headers.get(h) != null) out.push([h.replace(/(^|-)./g, (c) => c.toUpperCase()), r.headers.get(h)]);
    // The length as served, unless fetch decoded a compressed body (then
    // it is the compressed one's): otherwise chunked.
    // A HEAD of one whose length is not known says none.
    const len = r.headers.get('content-encoding') ? null : r.headers.get('content-length');
    const chunked = len == null && req.method === 'GET' && r.status !== 304;
    if (len != null) out.push(['Content-Length', len]);
    else if (chunked) out.push(['Transfer-Encoding', 'chunked']);
    else if (req.method !== 'HEAD') out.push(['Content-Length', '0']);
    this.#head(r.status, [...out, ...conn]);
    if (req.method === 'GET' && r.body && r.status !== 304) {
      this.#reader = r.body.getReader();
      for (;;) {
        const { done, value } = await this.#reader.read();
        if (done || this.#closed) break;
        if (!value?.length) continue;
        if (chunked) this.#emit(enc.encode(`${value.length.toString(16)}\r\n`));
        this.#emit(value);
        if (chunked) this.#emit(enc.encode('\r\n'));
      }
      this.#reader = null;
      if (chunked) this.#emit(enc.encode('0\r\n\r\n'));
    } else {
      await r.body?.cancel().catch(() => {});
    }
    if (!keep) this.#end();
  }
}

/** Where the head ends (the index of its blank line's `\r\n\r\n`), or -1. */
function headEnd(b) {
  for (let i = 0; i + 3 < b.length; i++) {
    if (b[i] === 13 && b[i + 1] === 10 && b[i + 2] === 13 && b[i + 3] === 10) return i;
  }
  return -1;
}

function parse(head) {
  const [line, ...rest] = head.split('\r\n');
  const [method, target, version] = line.split(' ');
  const headers = new Map();
  for (const l of rest) {
    const at = l.indexOf(':');
    if (at > 0) headers.set(l.slice(0, at).trim().toLowerCase(), l.slice(at + 1).trim());
  }
  const body = Number(headers.get('content-length') ?? 0) > 0 || headers.has('transfer-encoding');
  return { method, target: target ?? '', version: version ?? '', headers, body, bad: !method || !target || !/^HTTP\/1\.[01]$/.test(version ?? '') };
}
