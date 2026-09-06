// A minimal Chrome DevTools Protocol client, zero dependencies (Node's global
// `WebSocket`, v22+). Enough to drive the persistent browser `drive.mjs` keeps
// warm: one connection, request/response correlation, event subscription, and
// -- the reason this exists rather than a `--headless URL` spawn -- FLAT
// auto-attach, so a dedicated Worker's console and exceptions arrive on this
// same socket tagged with a `sessionId`. gex runs every guest process in a
// Worker, so a trap that matters happens THERE; without flat mode it is
// invisible.
//
// Events are delivered as `(params, sessionId)` -- `sessionId` is undefined for
// the browser session, a string for an attached page/worker target. Commands
// take an optional `sessionId` to address a specific target.

/* Long enough that a slow page-load command is not cut off, short enough that
 * a wedged browser is reported inside one gate's patience rather than never.
 * GEX_CDP_TIMEOUT overrides it. */
const CDP_TIMEOUT_MS = Number(process.env.GEX_CDP_TIMEOUT || 45000);

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

  /**
   * Send a command; resolves with its result, rejects on protocol error, and
   * rejects by name if the browser never answers.
   *
   * The timeout is not belt-and-braces. A CDP command the browser accepts and
   * never replies to used to hang this client for ever, and every caller is
   * upstream of drive.mjs's own --wall/--idle timers, which are armed later --
   * so the whole gate stopped with no output at all, no diagnosis, and the
   * same appearance whatever the cause. That happened for a week's worth of
   * runs and read as "the driver broke the browser" every time. A named
   * rejection naming the method is the difference between a mystery and a
   * fact.
   */
  send(method, params = {}, sessionId, timeoutMs = CDP_TIMEOUT_MS) {
    if (this.#closed) return Promise.reject(this.#closed);
    const id = ++this.#id;
    const payload = { id, method, params };
    if (sessionId) payload.sessionId = sessionId;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        if (!this.#pending.delete(id)) return;
        reject(new Error(
          `cdp: ${method} did not answer in ${timeoutMs}ms` +
          (sessionId ? ` (session ${sessionId})` : '')));
      }, timeoutMs);
      // Unref so a pending command cannot by itself keep node alive.
      timer.unref?.();
      this.#pending.set(id, {
        resolve: (v) => { clearTimeout(timer); resolve(v); },
        reject: (e) => { clearTimeout(timer); reject(e); },
      });
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
