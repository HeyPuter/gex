// The X server, in the page: a Worker for `hostx.js`.
//
// The machinery is in that module (which Node's `web/test/hostx.mjs` drives the
// same way); this is only the message plumbing. Note that after `start` this
// thread belongs to the X server -- `main()` does not return -- so a message
// sent afterwards is never read: the output size and the page's input travel
// through the shared arena instead, which is exactly how a guest process's
// compositor gets them too.

import { HostX } from './hostx.js';

let host = null;
onmessage = async (e) => {
  const m = e.data;
  if (m?.kind !== 'start' || host) return;
  host = new HostX((msg, transfer) => (transfer ? postMessage(msg, transfer) : postMessage(msg)));
  try {
    await host.start(m);
  } catch (err) {
    host.log(`[hostx] ${err?.stack || err}`);
    postMessage({ kind: 'failed', why: String(err?.message || err) });
  }
};
