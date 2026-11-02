// `Wisp`: a network (README "Network") over a Wisp proxy: every connection
// a stream on one WebSocket, with MercuryWorkshop's wisp-js client
// (vendor/wisp-js, see its SOURCE). The only module that imports it, so a
// page that does not import this carries none of it.

import { ClientConnection } from './vendor/wisp-js/client/connection.mjs';

/** How long the proxy has to answer a new WebSocket. */
const HELLO_MS = 10000;

export class Wisp extends EventTarget {
  #url;
  /** The WebSocket's connection once the proxy said hello, as a promise;
   *  null until the next connection dials. */
  #conn = null;

  /** `url`: the proxy (`ws:` or `wss:`), dialled at the first connection
   *  and again at the next one after it drops. Events: `open`, `closed`. */
  constructor(url) {
    super();
    let u = new URL(String(url)).href;
    if (!/^wss?:$/.test(new URL(u).protocol)) throw new TypeError(`Wisp: ${u} is not a ws: or wss: URL`);
    // The client insists on the slash.
    if (!u.endsWith('/')) u += '/';
    this.#url = u;
  }

  get url() {
    return this.#url;
  }

  /** A TCP connection to `host:port` (README "Network" `connect`). */
  connect(host, port) {
    return this.#stream(host, port, 'tcp');
  }

  /** UDP to `host:port`, one datagram a chunk (README "Network" `datagram`). */
  datagram(host, port) {
    return this.#stream(host, port, 'udp');
  }

  /** Close the WebSocket; the next connection dials again. */
  close() {
    const c = this.#conn;
    this.#conn = null;
    c?.then((conn) => conn.close(), () => {});
  }

  async #stream(host, port, type) {
    const conn = await this.#dial();
    const s = conn.create_stream(String(host), Number(port), type);
    const readable = new ReadableStream({
      start(c) {
        s.onmessage = (data) => c.enqueue(new Uint8Array(data));
        s.onclose = () => { try { c.close(); } catch {} };
      },
      cancel: () => s.close(),
    });
    const writable = new WritableStream({
      write: (bytes) => s.send(bytes),
      close: () => s.close(),
      abort: () => s.close(),
    });
    return { readable, writable };
  }

  #dial() {
    if (this.#conn) return this.#conn;
    const made = new Promise((resolve, reject) => {
      const c = new ClientConnection(this.#url);
      let up = false;
      const timer = setTimeout(() => { if (!up) { c.onclose = c.onerror = () => {}; c.close(); down(new Error(`${this.#url} did not answer`)); } }, HELLO_MS);
      const down = (e) => {
        clearTimeout(timer);
        if (this.#conn === made) this.#conn = null;
        if (up) this.dispatchEvent(new Event('closed'));
        else reject(new Error(`Wisp: ${e?.message ?? `could not reach ${this.#url}`}`));
      };
      c.onopen = () => {
        up = true;
        clearTimeout(timer);
        resolve(c);
        this.dispatchEvent(new Event('open'));
      };
      c.onclose = () => down();
      c.onerror = () => down();
    });
    this.#conn = made;
    return made;
  }
}
