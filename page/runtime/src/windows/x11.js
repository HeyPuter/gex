// X11 for every windows part: display :0 held by the page (programs.js) until
// the first X client connects, then the runtime's X server (page/src/hostx.js,
// runtime.js `startXServer`) started on those sockets, where the client
// waits.

/** Arm it for a system's internals `i`, once the system runs. `x`: what a
 *  preset asks of the server (page/src/run-program.js): `rootful`, one
 *  fullscreen X screen under the session's own window manager (hostx.js). */
export function xOnDemand(i, x = {}) {
  i.afterBoot(async (sys) => {
    // The single-thread build's server is in its system Worker (`startX`).
    if (!sys.wlChannel && !sys.startX) return;
    const ids = await i.programs().holdX();
    let server = null;
    i.on('x-wanted', () => {
      server ??= sys.startX
        ? sys.startX({ listen: ids, rootful: !!x.rootful })
        : sys.rt.startXServer({ kernel: sys.kernel, wlChannel: sys.wlChannel, compositor: () => sys.compositor, term: sys.term, listen: ids, rootful: !!x.rootful });
      server.catch((e) => i.say(`gex: the X server would not start: ${e?.message ?? e}\n`));
    });
    // Its Worker's `main` never returns: ending the system ends the Worker.
    sys.onStop(() => server?.then((w) => w?.terminate?.(), () => {}));
  });
}
