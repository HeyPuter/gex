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
/// What the proxy delivers, counted. See the `onmessage` below.
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

  /** Open the WebSocket. Resolves once the proxy has said hello. */
  /**
   * The proxy this was pointed at, normalised.
   *
   * A guest process is a Worker now, and each opens its own connection to
   * the proxy rather than relaying through the page -- a socket is per
   * process on a real machine too. So the address is what travels, and this
   * is where a page that has already made one gets it from.
   */
  get url() {
    return this.#url;
  }

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
      // When the bytes stop, and for how long.
      //
      // `apt-get update` reports 14.9 MB at 44 kB/s, which reads like a slow
      // transport and is not one. The timeline says otherwise:
      //
      //     16 messages,  0.0 MiB in   2.3s
      //     32 messages,  0.1 MiB in   2.3s
      //     48 messages,  0.2 MiB in 320.9s
      //    384 messages,  7.5 MiB in 321.7s
      //
      // One stall of 318 seconds after about a hundred kilobytes, and then
      // seven megabytes in eight tenths of a second -- nine megabytes a
      // second, which is the speed of the machine. The average was hiding a
      // hang, and a hang of about five minutes that ends by itself is a
      // timeout expiring rather than a bandwidth problem.
      //
      // So this reports gaps rather than rates: silence is the signal.
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
    // A copy, because the argument is a view into the guest's memory and the
    // client keeps it until the socket drains.
    // When the request went out, against when the first byte came back.
    //
    // The stall is 318 seconds of silence on a transport that then runs at
    // nine megabytes a second, and the guest side is accounted for: apt is
    // polling its methods' pipes (1 reader, 1 writer, empty) and the methods
    // are waiting on the network. So either the request leaves late, which
    // is ours, or it leaves promptly and the answer is late, which is the
    // proxy's or the mirror's. One number each way settles it.
    // Every send, not just the first.
    //
    // The stall is a fixed 318 seconds -- 318.1 and 318.4 across runs -- after
    // about a hundred kilobytes, and it survives the Worker polling fifty
    // times a second instead of thirty-two thousand, so it is not contention
    // and not starvation. A fixed five minutes that ends by itself is
    // somebody's idle timeout.
    //
    // apt keeps the connection alive and sends its next request on it. The
    // probe that does *not* stall sends one request and `Connection: close`.
    // So the question is whether the second request leaves at all: if apt
    // sends at three seconds and the answer arrives at three hundred and
    // twenty, the send is being lost and the server is idling until it gives
    // up.
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
