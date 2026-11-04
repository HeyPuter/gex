// A minimal Chrome DevTools Protocol client, zero dependencies (Node's global
// `WebSocket`, v22+). Enough to drive a persistent browser: one connection,
// request/response correlation, event subscription, and -- the reason this
// exists rather than a `--headless URL` spawn -- FLAT
// auto-attach, so a dedicated Worker's console and exceptions arrive on this
// same socket tagged with a `sessionId`. gex runs every guest process in a
// Worker, so a trap that matters happens THERE; without flat mode it is
// invisible.
//
// Events are delivered as `(params, sessionId)` -- `sessionId` is undefined for
// the browser session, a string for an attached page/worker target. Commands
// take an optional `sessionId` to address a specific target.

export class Cdp {
  #ws;
  #id = 0;
  #pending = new Map();
  #handlers = new Map();
  #closed = null;

  /** Connect to a `webSocketDebuggerUrl` (browser- or target-level). */
  static async connect(wsUrl) {
    const c = new Cdp();
    await c.#open(wsUrl);
    return c;
  }

  #open(wsUrl) {
    // Node's global (WHATWG) WebSocket: `(url[, protocols])`, no options object
    // -- CDP frames are small JSON text frames, so the defaults are fine.
    this.#ws = new WebSocket(wsUrl);
    this.#ws.addEventListener('message', (e) => this.#onMessage(e.data));
    const done = new Promise((resolve, reject) => {
      this.#ws.addEventListener('open', () => resolve(), { once: true });
      this.#ws.addEventListener('error', (e) => reject(new Error('cdp socket error')), {
        once: true,
      });
    });
    // A closed socket must reject every in-flight command, or a caller awaits a
    // reply that can never come -- a hang disguised as a slow browser.
    this.#ws.addEventListener('close', () => {
      const err = new Error('cdp socket closed');
      this.#closed = err;
      for (const { reject } of this.#pending.values()) reject(err);
      this.#pending.clear();
    });
    return done;
  }

  #onMessage(data) {
    const msg = JSON.parse(typeof data === 'string' ? data : data.toString());
    if (msg.id !== undefined) {
      const p = this.#pending.get(msg.id);
      if (!p) return;
      this.#pending.delete(msg.id);
      if (msg.error) p.reject(new Error(`${msg.error.message} (${msg.error.code})`));
      else p.resolve(msg.result);
      return;
    }
    // An event. Fan out to every handler for this method; a handler that
    // throws must not take the socket down with it.
    const hs = this.#handlers.get(msg.method);
    if (hs) for (const h of hs) {
      try {
        h(msg.params ?? {}, msg.sessionId);
      } catch (e) {
        console.error(`[cdp] handler for ${msg.method} threw`, e);
      }
    }
  }

  /** Send a command; resolves with its result, rejects on protocol error. */
  send(method, params = {}, sessionId) {
    if (this.#closed) return Promise.reject(this.#closed);
    const id = ++this.#id;
    const payload = { id, method, params };
    if (sessionId) payload.sessionId = sessionId;
    return new Promise((resolve, reject) => {
      this.#pending.set(id, { resolve, reject });
      this.#ws.send(JSON.stringify(payload));
    });
  }

  /** Subscribe to an event method. Returns an unsubscribe function. */
  on(method, handler) {
    if (!this.#handlers.has(method)) this.#handlers.set(method, new Set());
    this.#handlers.get(method).add(handler);
    return () => this.#handlers.get(method)?.delete(handler);
  }

  close() {
    try {
      this.#ws.close();
    } catch {
      /* already gone */
    }
  }
}

/** Ask a running browser for its `webSocketDebuggerUrl`, or null if none. */
export async function browserWsUrl(port, timeoutMs = 1500) {
  try {
    const r = await fetch(`http://127.0.0.1:${port}/json/version`, {
      signal: AbortSignal.timeout(timeoutMs),
    });
    if (!r.ok) return null;
    return (await r.json()).webSocketDebuggerUrl ?? null;
  } catch {
    return null;
  }
}
