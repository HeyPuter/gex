// A UDP flow's datagrams on one of the shim's byte streams (net_connect with
// the datagram flag, shims/browser/src/lib.rs `PageDgram`): each its length,
// two bytes big-endian, then its bytes, both ways.

/** The port flag of a datagram flow's `connect`. */
export const NET_DGRAM = 0x10000;

/** `bytes` as one frame. */
export function frame(bytes) {
  const out = new Uint8Array(2 + bytes.length);
  out[0] = bytes.length >> 8;
  out[1] = bytes.length & 255;
  out.set(bytes, 2);
  return out;
}

/** The datagrams in a byte stream, as its pieces come. */
export class Unframer {
  #buf = new Uint8Array(0);

  /** The datagrams `bytes` completes. */
  push(bytes) {
    const b = new Uint8Array(this.#buf.length + bytes.length);
    b.set(this.#buf);
    b.set(bytes, this.#buf.length);
    const out = [];
    let at = 0;
    while (b.length - at >= 2) {
      const n = (b[at] << 8) | b[at + 1];
      if (b.length - at < 2 + n) break;
      out.push(b.slice(at + 2, at + 2 + n));
      at += 2 + n;
    }
    this.#buf = b.slice(at);
    return out;
  }
}
