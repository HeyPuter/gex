// What HostWindows and SingleCanvas share in the single-thread build.

/** The single-thread build's windows: the scene is the system Worker's, and
 *  the page's DomDesktop in `el` a mirror of it (runtime.js
 *  `openWaylandMirror`). `size` is the output's in device pixels, which the
 *  Worker sets before its first program. */
export function mirrored(dev, el, size, onDesktop) {
  dev.single.wl = {
    dom: true,
    ...size,
    attach: async (k, listeners) => {
      const r = await dev.rt.openWaylandMirror({ wl: el, post: (fn, args) => k.post({ wl: { fn, args } }), onDesktop, say: (t) => dev.say(`${t}\n`) });
      listeners.add((ev) => {
        if (ev.ev === 'wl-mirror') r.apply(ev.update);
        else if (ev.ev === 'wl-drop') r.compositor.dropMemory(ev.token);
      });
      return r;
    },
  };
}
