// The guest's sockets as every Worker reaches them (served by
// hosts/browser/js/net-bridge.mjs), standing for the system's socket table
// (page/src/net.js `Sockets`), made when the page first sets a network: until
// then every connection out is unreachable.

const CLOSED = 2;

export class NetSwitch {
  #b = null;
  #rings = null;
  onActivity = () => {};

  /** The socket table (page/src/net.js `Sockets`). */
  set(backend) {
    this.#b = backend;
    if ('rings' in backend) backend.rings = this.#rings;
    backend.onActivity = (...a) => this.onActivity(...a);
  }

  // net-bridge.mjs sets and clears the receive rings.
  get rings() {
    return this.#rings;
  }
  set rings(r) {
    this.#rings = r;
    if (this.#b && 'rings' in this.#b) this.#b.rings = r;
  }

  get pageHosts() {
    return this.#b?.pageHosts;
  }

  open() { return this.#b ? this.#b.open() : 0; }
  connect(id, host, port) { this.#b?.connect(id, host, port); }
  status(id) { return this.#b ? this.#b.status(id) : CLOSED; }
  send(id, bytes) { return this.#b ? this.#b.send(id, bytes) : -1; }
  recv(id, out) { return this.#b ? this.#b.recv(id, out) : 0; }
  peek(id, out) { return this.#b ? this.#b.peek(id, out) : 0; }
  frame(id, out, take) { return this.#b ? this.#b.frame(id, out, take) : 0; }
  available(id) { return this.#b ? this.#b.available(id) : true; }
  dup(id) { return this.#b ? this.#b.dup(id) : 0; }
  close(id) { this.#b?.close(id); }
  refill(id) { return this.#b?.refill?.(id) ?? 0; }
}
