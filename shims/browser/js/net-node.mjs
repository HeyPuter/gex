// A network for the Node fast lane, backed by real TCP sockets: the same seven
// calls the page carries over Wisp, so everything above Wisp -- apt's helper,
// the fetch, the decompression, the parse -- runs here in two minutes instead
// of twenty. Wisp itself is not exercised.
//
// Every call returns without waiting: the guest's thread is parked by the
// kernel, so blocking here would take the wasm stack and nothing would ever
// deliver the bytes being waited for.
import { createConnection } from 'node:net';
import { readFile, stat } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { PageHostStream } from './page-host.mjs';

/** `fetch`, and for a `file:` URL the file itself (a page-served host's
 *  files in a directory): 404 where there is none. */
async function fetchOrFile(url, init) {
  if (!url.startsWith('file:')) return fetch(url, init);
  const path = fileURLToPath(url);
  try {
    const st = await stat(path);
    if (!st.isFile()) throw new Error('not a file');
    const body = init?.method === 'HEAD' ? null : await readFile(path);
    return new Response(body, { status: 200, headers: { 'content-length': String(st.size), 'last-modified': st.mtime.toUTCString() } });
  } catch {
    return new Response(null, { status: 404, headers: { 'content-length': '0' } });
  }
}

/** Status codes the shim understands. */
const CONNECTING = 0;
const OPEN = 1;
const REFUSED = 2;
const CLOSED = 3;

export class NodeNetwork {
  #sockets = new Map();
  #next = 1;
  /** The page-served hosts (page-host.mjs), answered here, not over TCP. */
  #pageHosts = null;

  /** Whether a host not page-served is reached over TCP (else refused). */
  #tcp;

  /** `pageHosts`: a Map of host name to base URL (`file:` for a directory). */
  constructor({ pageHosts = null, tcp = true } = {}) {
    this.#pageHosts = pageHosts?.size ? pageHosts : null;
    this.#tcp = tcp;
  }
  /** Called on every socket event: the single-thread build wakes its parked
   *  fibers with it (as web/src/net.js's WispNetwork does). */
  onActivity = () => {};

  /** A fresh identifier; the guest connects it next. */
  open() {
    const id = this.#next++;
    this.#sockets.set(id, { sock: null, status: CONNECTING, rx: [], eof: false, refs: 1 });
    return id;
  }

  connect(id, host, port) {
    const s = this.#sockets.get(id);
    if (!s) return;
    const base = this.#pageHosts?.get(String(host).toLowerCase());
    if (base) {
      const p = new PageHostStream(base, fetchOrFile);
      p.onmessage = (d) => {
        s.rx.push(d);
        this.onActivity();
      };
      p.onclose = () => {
        s.eof = true;
        this.onActivity();
      };
      s.sock = { write: (b) => p.send(new Uint8Array(b)), destroy: () => p.close() };
      s.status = OPEN;
      return;
    }
    if (!this.#tcp) {
      s.status = REFUSED;
      s.eof = true;
      return;
    }
    const sock = createConnection({ host, port }, () => {
      s.status = OPEN;
      this.onActivity();
    });
    sock.on('data', (d) => {
      s.rx.push(d);
      this.onActivity();
    });
    sock.on('end', () => {
      s.eof = true;
      this.onActivity();
    });
    // A refusal and a reset are the same to the guest: the connection is not
    // there. Distinguishing them would only matter if anything asked.
    sock.on('error', () => {
      s.status = s.status === CONNECTING ? REFUSED : CLOSED;
      s.eof = true;
      this.onActivity();
    });
    sock.on('close', () => {
      if (s.status === CONNECTING) s.status = REFUSED;
      s.eof = true;
      this.onActivity();
    });
    s.sock = sock;
  }

  status(id) {
    return this.#sockets.get(id)?.status ?? CLOSED;
  }

  send(id, bytes) {
    const s = this.#sockets.get(id);
    if (!s?.sock || s.status !== OPEN) return -1;
    // Copied, because the view is onto the guest's memory and Node keeps the
    // buffer until the write drains -- by which time the guest has moved on
    // and written something else there.
    s.sock.write(Buffer.from(bytes));
    return bytes.length;
  }

  recv(id, out) {
    const s = this.#sockets.get(id);
    if (!s) return 0;
    // Nothing buffered is not end of stream: 0 means end, -1 means "not yet".
    // Returning 0 for both let a guest that got ahead of the socket see the
    // connection finish early (busybox wget: `connection closed prematurely`).
    if (!s.rx.length) return s.eof || s.status === REFUSED ? 0 : -1;
    let n = 0;
    while (n < out.length && s.rx.length) {
      const head = s.rx[0];
      const take = Math.min(head.length, out.length - n);
      out.set(head.subarray(0, take), n);
      n += take;
      if (take === head.length) s.rx.shift();
      else s.rx[0] = head.subarray(take);
    }
    return n;
  }

  /** `recv`, leaving the bytes queued (web/src/net.js's `peek`). */
  peek(id, out) {
    const s = this.#sockets.get(id);
    if (!s) return 0;
    if (!s.rx.length) return s.eof || s.status === REFUSED ? 0 : -1;
    let n = 0;
    for (const c of s.rx) {
      if (n >= out.length) break;
      const take = Math.min(c.length, out.length - n);
      out.set(c.subarray(0, take), n);
      n += take;
    }
    return n;
  }

  /**
   * Whether a `recv` would return something -- including end of stream, which
   * is a thing to return rather than a reason to keep waiting.
   */
  available(id) {
    const s = this.#sockets.get(id);
    return !!s && (s.rx.length > 0 || s.eof || s.status === REFUSED);
  }

  /** Another holder (web/src/net.js's `dup`): the last `close` ends it. */
  dup(id) {
    const s = this.#sockets.get(id);
    if (!s) return 0;
    s.refs++;
    return 1;
  }

  close(id) {
    const s = this.#sockets.get(id);
    if (s && --s.refs > 0) return;
    s?.sock?.destroy();
    this.#sockets.delete(id);
  }
}
