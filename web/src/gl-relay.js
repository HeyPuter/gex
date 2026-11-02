// The page's side of the single-thread build's GL beside a DOM desktop: what
// DomDesktop calls on a Mesa device (dom-compositor.js `hostGl`'s `device`),
// with Mesa in the system Worker (gl-in-realm.js). A window's GL canvas is
// handed over there as an OffscreenCanvas the first time it is named.

/** `k`: the page's Kernel for that Worker (`ask`, `post`); `listeners`: its
 *  events; `opts`: Mesa's `{ url, files, api }`. */
export function relayedGl(k, listeners, opts) {
  let list = [];
  let state = { ok: false, error: null, describe: null };
  const keys = new WeakMap();
  const els = new Map();
  let next = 1;
  listeners.add((ev) => {
    if (ev.ev === 'gl-surfaces') list = ev.list;
  });
  const device = {
    get ok() {
      return state.ok;
    },
    get error() {
      return state.error;
    },
    describe: () => state.describe,
    surfaces: () => list,
    surfaceCanvas(frame, on) {
      const el = on ? document.getElementById(`gex-gl-${frame}`) : null;
      if (!on) {
        k.post({ glc: { frame, on: false } });
        // A canvas the desktop took away (`dropGl`) is let go there too.
        setTimeout(() => {
          for (const [key, e] of els) {
            if (!e.isConnected) {
              els.delete(key);
              k.post({ glc: { gone: key } });
            }
          }
        }, 0);
        return;
      }
      if (!el) return;
      let key = keys.get(el);
      if (key) return void k.post({ glc: { frame, on: true, key } });
      key = next++;
      keys.set(el, key);
      els.set(key, el);
      const canvas = el.transferControlToOffscreen();
      k.post({ glc: { frame, on: true, key, canvas } }, [canvas]);
    },
    // The Worker forgets a process's GL when its memory goes, and presents
    // on its own clock.
    forgetClient() {},
    display() {},
    dispose() {
      k.post({ glc: { stop: true } });
    },
    async start() {
      state = await k.ask('glStart', opts);
      return device;
    },
  };
  return device;
}
