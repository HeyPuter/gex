// `Wisp`: the guest's connections out over a Wisp proxy (one WebSocket the
// streams share), gex's own network client (web/src/net.js).

import { Network } from '../net.js';
import { NET_BACKEND, NET_EVENT } from '../internal.js';

/** The proxy a page gets when it names none. */
export const DEFAULT_WISP = 'wss://anura.pro/';

export class Wisp extends Network {
  /** `url`: the proxy (default wss://anura.pro/). Events: `open` and
   *  `closed` as the WebSocket comes and goes (it is dialled again), and
   *  `connect` (`host`, `port`). */
  constructor(sys, { url = DEFAULT_WISP } = {}) {
    super(sys, { url: String(url) });
    this.#url = String(url);
  }

  // Set before Network makes its backend: that waits for a module import.
  #url;

  /** The proxy. */
  get url() {
    return this.#url;
  }

  [NET_BACKEND](WispNetwork) {
    return new WispNetwork(this.url, (e) => this[NET_EVENT](e));
  }
}
