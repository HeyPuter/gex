// `Wisp`: the guest's connections out over a Wisp proxy (one WebSocket the
// streams share), gex's own network client (web/src/net.js).

import { Network } from '../net.js';

/** The proxy a page gets when it names none. */
export const DEFAULT_WISP = 'wss://anura.pro/';

export class Wisp extends Network {
  /** `url`: the proxy (default wss://anura.pro/). Events: `open` and
   *  `closed` as the WebSocket comes and goes (it is dialled again), and
   *  `connect` (`host`, `port`). */
  constructor(sys, { url = DEFAULT_WISP } = {}) {
    super(sys, { url: String(url) });
  }

  get url() {
    return this.options.url;
  }

  makeBackend(WispNetwork) {
    return new WispNetwork(this.options.url, (e) => this.event(e));
  }
}
