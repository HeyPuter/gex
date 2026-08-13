// The guest's network, carried over Wisp: a page cannot open a TCP connection,
// so `@mercuryworkshop/wisp-js` multiplexes streams down one WebSocket to a
// proxy that can. This adds the shape the shim asks for: integer stream ids, a
// receive buffer per stream, and seven calls that never wait -- they must not,
// because the page's event loop is what delivers the bytes the parked guest is
// waiting on.

import { client } from '@mercuryworkshop/wisp-js/client';

const CONNECTING = 0;
const OPEN = 1;
const CLOSED = 2;
const REFUSED = 3;

/** How much may pile up unread on one stream before the page stops reading. */
const HIGH_WATER = 4 << 20;

class Stream {
  constructor(id) {
    this.id = id;
    this.state = CONNECTING;
    this.chunks = [];
    this.buffered = 0;
    this.wisp = null;
  }

  take(out) {
    let at = 0;
    while (at < out.length && this.chunks.length) {
      const head = this.chunks[0];
      const n = Math.min(out.length - at, head.length);
      out.set(head.subarray(0, n), at);
      at += n;
      if (n === head.length) this.chunks.shift();
      else this.chunks[0] = head.subarray(n);
      this.buffered -= n;
    }
    return at;
  }
}

// A network for `Gex.useNetwork`. `wispUrl` must end in a slash (the Wisp
// client insists). `meter` counts what the proxy delivers; see `onmessage`.
const meter = { n: 0, bytes: 0, last: 0 };

export class WispNetwork {
  #conn = null;
  #streams = new Map();
  #next = 1;
  #url;
  #onEvent;

  constructor(wispUrl, onEvent = () => {}) {
    this.#url = wispUrl.endsWith('/') ? wispUrl : `${wispUrl}/`;
    this.#onEvent = onEvent;
  }

  /** The proxy this was pointed at, normalised. Each guest process (a Worker)
   * opens its own connection to it, so the address is what travels between
   * them. */
  get url() {
    return this.#url;
  }

  /** Open the WebSocket. Resolves once the proxy has said hello. */
  async start() {
    if (this.#conn) return;
    await new Promise((resolve, reject) => {
      const c = new client.ClientConnection(this.#url);
      const fail = (e) => reject(new Error(`wisp: ${e?.message || 'could not connect'}`));
      c.onopen = () => {
        this.#conn = c;
        resolve();
      };
      c.onerror = fail;
      c.onclose = () => {
        // Every stream on a dropped connection is gone with it.
        for (const s of this.#streams.values()) s.state = CLOSED;
        this.#conn = null;
        this.#onEvent('closed');
      };
      setTimeout(() => reject(new Error('wisp: the proxy did not answer')), 10000);
    });
    this.#onEvent('open');
  }

  // -- the seven calls the shim makes --------------------------------------

  open() {
    if (!this.#conn) return 0;
    const id = this.#next++;
    this.#streams.set(id, new Stream(id));
    return id;
  }

  /** The shim's `net_connect`: point an already-opened stream at a host. */
  connect(id, host, port) {
    const s = this.#streams.get(id);
    if (!s || !this.#conn) return;
    let w;
    try {
      w = this.#conn.create_stream(host, port, 'tcp');
    } catch {
      s.state = REFUSED;
      return;
    }
    s.wisp = w;
    // The Wisp client reports the connection as open once the proxy has it.
    w.onopen = () => {
      s.state = OPEN;
    };
    w.onmessage = (data) => {
      const bytes = data instanceof Uint8Array ? data : new Uint8Array(data);
      // Report gaps, not rates: a low average was hiding a fixed ~318s stall
      // (an idle timeout), so silence is the signal worth logging.
      const now = performance.now();
      if (!s.firstRecv) {
        s.firstRecv = now;
        const waited = s.firstSend ? (now - s.firstSend) / 1000 : -1;
        console.error(
          `[wisp] stream ${id}: first reply at ${(now / 1000).toFixed(1)}s, `
          + `${waited.toFixed(1)}s after the request`,
        );
      }
      if (meter.last && now - meter.last > 1000) {
        console.error(
          `[wisp] nothing for ${((now - meter.last) / 1000).toFixed(1)}s `
          + `after ${meter.n} messages and `
          + `${(meter.bytes / 1048576).toFixed(1)} MiB`,
        );
      }
      meter.last = now;
      meter.n += 1;
      meter.bytes += bytes.length;
      if (s.buffered > HIGH_WATER) return;
      s.chunks.push(bytes);
      s.buffered += bytes.length;
    };
    w.onclose = () => {
      s.state = s.state === CONNECTING ? REFUSED : CLOSED;
    };
    w.onerror = () => {
      s.state = s.state === CONNECTING ? REFUSED : CLOSED;
    };
    // Some builds of the client open synchronously and never fire `onopen`.
    if (w.open === true || s.wisp?.open === true) s.state = OPEN;
    this.#onEvent(`connect ${host}:${port}`);
  }

  status(id) {
    const s = this.#streams.get(id);
    if (!s) return CLOSED;
    // Data that has arrived proves the connection came up, whatever the
    // client did or did not tell us.
    if (s.state === CONNECTING && s.chunks.length) s.state = OPEN;
    return s.state;
  }

  send(id, bytes) {
    const s = this.#streams.get(id);
    if (!s || !s.wisp || s.state === CLOSED || s.state === REFUSED) return -1;
    // Log every send with its timestamp: paired against the first-reply time in
    // `onmessage`, it says whether the ~318s stall is a late request (ours) or
    // a late answer (the proxy's or mirror's). The copy below is because the
    // argument is a view into guest memory the client keeps until the socket
    // drains.
    s.sends = (s.sends ?? 0) + 1;
    if (!s.firstSend) s.firstSend = performance.now();
    console.error(
      `[wisp] stream ${id}: send ${s.sends}, ${bytes.length} bytes at `
      + `${(performance.now() / 1000).toFixed(1)}s`,
    );
    s.wisp.send(bytes.slice());
    return bytes.length;
  }

  recv(id, out) {
    const s = this.#streams.get(id);
    if (!s) return 0;
    if (!s.chunks.length) return s.state === CLOSED || s.state === REFUSED ? 0 : -1;
    return s.take(out);
  }

  available(id) {
    const s = this.#streams.get(id);
    if (!s) return true;
    return s.chunks.length > 0 || s.state === CLOSED || s.state === REFUSED;
  }

  close(id) {
    const s = this.#streams.get(id);
    if (!s) return;
    try {
      s.wisp?.close();
    } catch {
      /* already gone */
    }
    this.#streams.delete(id);
  }
}
