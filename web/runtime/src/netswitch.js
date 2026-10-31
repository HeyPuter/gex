// The network runProgram is given at boot (its `network`, served to every
// Worker by shims/browser/js/net-bridge.mjs), standing for the one the page
// sets later (net.js): until then every connection out is unreachable.

const CLOSED = 2;

export class NetSwitch {
  #b = null;
  #rings = null;
  onActivity = () => {};

  /** The network from now on (a web/src/net.js WispNetwork). */
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

  get url() {
    return this.#b?.url ?? null;
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
  available(id) { return this.#b ? this.#b.available(id) : true; }
  dup(id) { return this.#b ? this.#b.dup(id) : 0; }
  close(id) { this.#b?.close(id); }
  refill(id) { return this.#b?.refill?.(id) ?? 0; }
}
