// The guest's network, carried over Wisp.
//
// A page cannot open a TCP connection. Wisp multiplexes streams down one
// WebSocket to a proxy that can, and `@mercuryworkshop/wisp-js` speaks it.
// What this adds is the shape the shim asks for: small integer stream ids,
// a receive buffer per stream, and seven calls that never wait.
//
// Never waiting is the whole constraint. The guest thread that asked for the
// bytes is parked by the kernel, and the page's event loop is what makes the
// bytes arrive -- so if any of these blocked, nothing would ever deliver
// them.

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

/**
 * A network for `Gex.useNetwork`.
 *
 * `wispUrl` is the proxy's endpoint and must end in a slash, which is what
 * the Wisp client insists on.
 */
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
    // A copy, because the argument is a view into the guest's memory and the
    // client keeps it until the socket drains.
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
