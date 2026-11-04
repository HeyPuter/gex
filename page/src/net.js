// The guest's sockets, over the page's network: a page cannot open a TCP
// connection, so each connection a program makes goes to the network the
// page gave the system (page/runtime README "Network": any object with
// `connect(host, port)` resolving `{ readable, writable }`, and
// `datagram(host, port)` for UDP, a datagram a chunk). A UDP flow is a
// stream here as a connection is, its datagrams framed (dgram-frames.mjs).
// This adds the shape the shim asks for: integer stream ids, a receive
// buffer per stream, and calls that never wait -- they must not, because the page's event loop
// is what delivers the bytes the parked guest is waiting on.

import { logOn } from '../../hosts/browser/js/log.mjs';
import { PageHostStream } from '../../hosts/browser/js/page-host.mjs';
import { NET_DGRAM, frame, frameOf, Unframer } from '../../hosts/browser/js/dgram-frames.mjs';

const CONNECTING = 0;
const OPEN = 1;
const CLOSED = 2;
const REFUSED = 3;

/** How much may pile up unread on one stream before the page says so. The
 *  network's stream is read as fast as it comes, so past this the bytes are
 *  kept anyway: dropping them is what turned a 9.6 MB download into a Hash
 *  Sum mismatch. */
const HIGH_WATER = 16 << 20;

class Stream {
  constructor(id) {
    this.id = id;
    this.state = CONNECTING;
    this.chunks = [];
    this.buffered = 0;
    this.conn = null;
    // Descriptors holding the stream, across processes (`dup`): closed when
    // the last lets go, as a socket outlives one of its descriptors.
    this.refs = 1;
    // Its receive ring's slot (`Sockets#rings`), or -1: `chunks` then
    // holds only what arrived past what the ring had room for.
    this.slot = -1;
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

  /** `take` without taking: what is queued, left queued (`recv(MSG_PEEK)`). */
  peek(out) {
    let at = 0;
    for (const c of this.chunks) {
      if (at >= out.length) break;
      const n = Math.min(out.length - at, c.length);
      out.set(c.subarray(0, n), at);
      at += n;
    }
    return at;
  }
}

/** A connection's two streams (a Duplex) as a stream the socket table
 *  wires (`#wire`): `send`, `close`, and `onopen`, `onmessage`, `onclose`,
 *  `onerror`. */
class DuplexWire {
  open = false;
  onopen = null;
  onmessage = null;
  onclose = null;
  onerror = null;
  #writer = null;
  #reader = null;
  #pending = [];
  #closed = false;

  constructor(made) {
    Promise.resolve(made).then(async ({ readable, writable }) => {
      if (this.#closed) {
        readable?.cancel?.().catch(() => {});
        writable?.abort?.().catch(() => {});
        return;
      }
      this.#writer = writable.getWriter();
      for (const b of this.#pending.splice(0)) this.#writer.write(b).catch(() => {});
      this.open = true;
      this.onopen?.();
      this.#reader = readable.getReader();
      try {
        for (;;) {
          const { value, done } = await this.#reader.read();
          if (done) break;
          if (value?.length) this.onmessage?.(value instanceof Uint8Array ? value : new Uint8Array(value));
        }
      } catch {}
      this.#closed = true;
      this.onclose?.();
    }, (e) => {
      this.#closed = true;
      this.onerror?.(e);
    });
  }

  send(bytes) {
    if (this.#closed) return;
    if (this.#writer) this.#writer.write(bytes).catch(() => {});
    else this.#pending.push(bytes);
  }

  close() {
    if (this.#closed) return;
    this.#closed = true;
    this.#writer?.close().catch(() => {});
    this.#reader?.cancel().catch(() => {});
  }
}

/** The page's network as a system Worker reaches it, the network staying
 *  on the page: each connection asked for there (`post(m, transfer)`, the
 *  page answering with `dialFor`), its streams transferred here.
 *  `datagram`: whether the page's network has `datagram`. */
export function pageRelay(post, { datagram }) {
  const dial = (host, port, dgram) => new Promise((resolve, reject) => {
    const { port1, port2 } = new MessageChannel();
    port1.onmessage = ({ data }) => {
      port1.close();
      if (data.error) reject(new Error(data.error));
      else resolve({ readable: data.readable, writable: data.writable });
    };
    post({ host, port, datagram: dgram, reply: port2 }, [port2]);
  });
  const relay = { connect: (host, port) => dial(host, port, false) };
  if (datagram) relay.datagram = (host, port) => dial(host, port, true);
  return relay;
}

/** The page's side of `pageRelay`: a connection of the page's network
 *  `net` for request `m`, its streams handed to the Worker (transferred),
 *  or why not. */
export function dialFor(net, m) {
  const reply = m.reply;
  Promise.resolve()
    .then(() => {
      if (!net) throw new Error('no network');
      if (m.datagram && typeof net.datagram !== 'function') throw new Error('no datagrams');
      return m.datagram ? net.datagram(m.host, m.port) : net.connect(m.host, m.port);
    })
    .then(({ readable, writable }) => reply.postMessage({ readable, writable }, [readable, writable]),
      (e) => reply.postMessage({ error: String(e?.message ?? e) }));
}

// `meter` counts what the network delivers; see `onmessage`.
const meter = { n: 0, bytes: 0, last: 0 };

/** The guest's sockets (`Gex.useNetwork`, net-bridge.mjs `serveNet`). */
export class Sockets {
  #streams = new Map();
  #next = 1;
  #onEvent;
  /** The page-served hosts (page-host.mjs `pageHosts`): name -> base URL. */
  #pageHosts = null;
  /** The page's network (`connect(host, port)`), or null. */
  #network = null;

  /** Receive rings in shared memory (net-bridge.mjs `Rings`), set by
   *  `serveNet`: each stream's bytes go into its ring as they arrive, and its
   *  Workers read them there without asking the page. Null: none. */
  rings = null;

  /** Called when a stream opens, receives or closes. The single-thread
   *  build wakes its parked fibers here (a guest waiting on a socket sleeps
   *  on the scheduler, not on this thread's event loop). */
  onActivity = () => {};

  /** `network`: the page's (`connect(host, port)`), or null. `pageHosts`: a
   *  Map of host name to the base URL its files are fetched under
   *  (page-host.mjs), answered here rather than by the network. `onEvent`:
   *  told 'connect HOST:PORT' for each connection. */
  constructor(network, { pageHosts = null, onEvent = () => {} } = {}) {
    this.#network = network ?? null;
    this.#onEvent = onEvent;
    this.#pageHosts = pageHosts?.size ? pageHosts : null;
  }

  /** The page's network: connections opened from now on go to it, and
   *  open ones stay on the one they were opened on. */
  get network() {
    return this.#network;
  }
  set network(network) {
    this.#network = network ?? null;
  }

  /** The page-served hosts, as `[name, base]` pairs. */
  get pageHosts() {
    return this.#pageHosts ?? undefined;
  }

  // -- the seven calls the shim makes --------------------------------------

  open() {
    if (!this.#pageHosts && !this.#network) return 0;
    const id = this.#next++;
    const s = new Stream(id);
    if (this.rings) s.slot = this.rings.claim(id);
    this.#streams.set(id, s);
    return id;
  }

  /** Stream `s` in state `st`, in its ring too. */
  #state(s, st) {
    s.state = st;
    if (s.slot >= 0) this.rings?.setState(s.slot, st);
  }

  /** The shim's `net_connect`: point an already-opened stream at a host. */
  connect(id, host, port) {
    const s = this.#streams.get(id);
    if (!s) return;
    if (port & NET_DGRAM) {
      this.#datagram(s, id, host, port & 0xffff);
      return;
    }
    const base = this.#pageHosts?.get(String(host).toLowerCase());
    if (base) {
      this.#wire(s, id, new PageHostStream(base), host, port);
      return;
    }
    if (!this.#network) {
      this.#state(s, REFUSED);
      return;
    }
    let w;
    try {
      w = new DuplexWire(this.#network.connect(String(host), Number(port)));
    } catch {
      this.#state(s, REFUSED);
      return;
    }
    this.#wire(s, id, w, host, port);
  }

  /** Stream `s` as a UDP flow to `host:port`: the network's `datagram`,
   *  each datagram framed on the stream (refused by a network without it). */
  #datagram(s, id, host, port) {
    let w;
    try {
      if (typeof this.#network?.datagram !== 'function') throw new Error('no datagrams');
      w = new DuplexWire(this.#network.datagram(String(host), Number(port)));
    } catch {
      this.#state(s, REFUSED);
      return;
    }
    const out = new Unframer();
    const wire = {
      get open() { return w.open; },
      send: (bytes) => { for (const d of out.push(bytes)) w.send(d); },
      close: () => w.close(),
    };
    this.#wire(s, id, wire, host, port);
    for (const k of ['onopen', 'onclose', 'onerror']) w[k] = (...a) => wire[k]?.(...a);
    w.onmessage = (d) => wire.onmessage?.(frame(d));
  }

  /** Stream `s` carried by `w`: the network's connection, or a page-served host's. */
  #wire(s, id, w, host, port) {
    s.conn = w;
    // Open once the network has given its streams.
    w.onopen = () => {
      this.#state(s, OPEN);
      this.onActivity();
    };
    w.onmessage = (data) => {
      const bytes = data instanceof Uint8Array ? data : new Uint8Array(data);
      // Report gaps, not rates: a low average was hiding a fixed ~318s stall
      // (an idle timeout), so silence is the signal worth logging.
      const now = performance.now();
      if (!s.firstRecv) {
        s.firstRecv = now;
        const waited = s.firstSend ? (now - s.firstSend) / 1000 : -1;
        // Per-stream timing: debug output (GEX_LOG, `?log=1`).
        if (logOn('debug')) console.info(
          `[net] stream ${id}: first reply at ${(now / 1000).toFixed(1)}s, `
          + `${waited.toFixed(1)}s after the request`,
        );
      }
      if (meter.last && now - meter.last > 1000) {
        if (logOn('debug')) console.info(
          `[net] nothing for ${((now - meter.last) / 1000).toFixed(1)}s `
          + `after ${meter.n} messages and `
          + `${(meter.bytes / 1048576).toFixed(1)} MiB`,
        );
      }
      meter.last = now;
      meter.n += 1;
      meter.bytes += bytes.length;
      if (s.buffered > HIGH_WATER && !s.warned) {
        s.warned = true;
        console.warn(`[net] stream ${id}: ${s.buffered >> 20} MiB unread`);
      }
      this.#take(s, bytes);
      this.onActivity();
    };
    w.onclose = () => {
      this.#state(s, s.state === CONNECTING ? REFUSED : CLOSED);
      this.onActivity();
    };
    w.onerror = () => {
      this.#state(s, s.state === CONNECTING ? REFUSED : CLOSED);
      this.onActivity();
    };
    if (w.open === true) this.#state(s, OPEN);
    this.#onEvent(`connect ${host}:${port}`);
  }

  /** Bytes stream `s` received: into its ring as far as there is room
   *  and nothing is held back already, the rest kept. */
  #take(s, bytes) {
    if (s.slot >= 0 && !s.chunks.length) bytes = bytes.subarray(this.rings.write(s.slot, bytes));
    if (!bytes.length) return;
    s.chunks.push(bytes);
    s.buffered += bytes.length;
    if (s.slot >= 0) this.rings.setSpill(s.slot, true);
  }

  /** What stream `id` holds past its ring, into the ring as far as there
   *  is room (net-bridge.mjs `REFILL`). */
  refill(id) {
    const s = this.#streams.get(id);
    if (!s || s.slot < 0) return 0;
    while (s.chunks.length) {
      const head = s.chunks[0];
      const n = this.rings.write(s.slot, head);
      s.buffered -= n;
      if (n < head.length) {
        s.chunks[0] = head.subarray(n);
        break;
      }
      s.chunks.shift();
    }
    this.rings.setSpill(s.slot, s.chunks.length > 0);
    return 1;
  }

  status(id) {
    const ring = this.rings?.statusOf(id);
    if (ring !== undefined) return ring;
    const s = this.#streams.get(id);
    if (!s) return CLOSED;
    // Data that has arrived proves the connection came up, whatever the
    // client did or did not tell us.
    if (s.state === CONNECTING && s.chunks.length) s.state = OPEN;
    return s.state;
  }

  send(id, bytes) {
    const s = this.#streams.get(id);
    if (!s || !s.conn || s.state === CLOSED || s.state === REFUSED) return -1;
    // Log every send with its timestamp: paired against the first-reply time in
    // `onmessage`, it says whether the ~318s stall is a late request (ours) or
    // a late answer (the proxy's or mirror's). The copy below is because the
    // argument is a view into guest memory the client keeps until the socket
    // drains.
    s.sends = (s.sends ?? 0) + 1;
    if (!s.firstSend) s.firstSend = performance.now();
    if (logOn('debug')) console.info(
      `[net] stream ${id}: send ${s.sends}, ${bytes.length} bytes at `
      + `${(performance.now() / 1000).toFixed(1)}s`,
    );
    s.conn.send(bytes.slice());
    return bytes.length;
  }

  recv(id, out) {
    const ring = this.rings?.recv(id, out, false, () => this.refill(id));
    if (ring !== undefined) return ring;
    const s = this.#streams.get(id);
    if (!s) return 0;
    if (!s.chunks.length) return s.state === CLOSED || s.state === REFUSED ? 0 : -1;
    return s.take(out);
  }

  /** `recv`, leaving the bytes queued. Wine's wineserver asks it of every
   *  readable stream socket (one byte, MSG_PEEK), to tell data from the far
   *  end having closed. */
  peek(id, out) {
    const ring = this.rings?.recv(id, out, true, () => this.refill(id));
    if (ring !== undefined) return ring;
    const s = this.#streams.get(id);
    if (!s) return 0;
    if (!s.chunks.length) return s.state === CLOSED || s.state === REFUSED ? 0 : -1;
    return s.peek(out);
  }

  /** A UDP flow's next datagram, framed, whole or nothing (the shim's
   *  `net_frame`, dgram-frames.mjs `frameOf`): its length, -1 until all of
   *  it has arrived, 0 at the end. Taken unless `!take`. */
  frame(id, out, take) {
    const ring = this.rings?.frame(id, out, take, () => this.refill(id));
    if (ring !== undefined) return ring;
    return frameOf((o) => this.peek(id, o), (o) => this.recv(id, o), out, take);
  }

  available(id) {
    const ring = this.rings?.available(id);
    if (ring !== undefined) return ring;
    const s = this.#streams.get(id);
    if (!s) return true;
    return s.chunks.length > 0 || s.state === CLOSED || s.state === REFUSED;
  }

  /** Another holder of a stream (a descriptor passed to another process:
   *  SCM_RIGHTS, a fork): `close` then lets go of one hold, and only the last
   *  ends the stream. 1 if the stream exists, else 0. */
  dup(id) {
    const s = this.#streams.get(id);
    if (!s) return 0;
    s.refs++;
    return 1;
  }

  close(id) {
    const s = this.#streams.get(id);
    if (!s) return;
    if (--s.refs > 0) return;
    try {
      s.conn?.close();
    } catch {
      /* already gone */
    }
    this.#streams.delete(id);
    if (s.slot >= 0) this.rings?.release(s.slot);
  }
}
