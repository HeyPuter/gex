// X11 for every windows part: display :0 held by the page (programs.js) until
// the first X client connects, then the runtime's X server (web/src/hostx.js,
// runtime.js `startXServer`) started on those sockets, where the client
// waits.

/** Arm it for a system's internals `i`, once the system runs. */
export function xOnDemand(i) {
  i.afterBoot(async (sys) => {
    if (!sys.wlChannel) return;
    const ids = await i.programs().holdX();
    let x = null;
    i.on('x-wanted', () => {
      x ??= sys.rt.startXServer({ kernel: sys.kernel, wlChannel: sys.wlChannel, compositor: () => sys.compositor, term: sys.term, listen: ids });
      x.catch((e) => i.say(`gex: the X server would not start: ${e?.message ?? e}\n`));
    });
    // Its Worker's `main` never returns: ending the system ends the Worker.
    sys.onStop(() => x?.then((w) => w.terminate(), () => {}));
  });
}
